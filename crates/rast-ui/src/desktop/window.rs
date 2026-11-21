//! Classic desktop window chrome and the client-side application views.

use rune::{Align, Color, El, LiveRegion, ScrollSpec, Shape, TextStyle, col, el, row, text};

use super::{AppId, Palette, icon};
use crate::{State, connection::Phase};

const TASKBAR: f32 = 54.0;
const LABEL: TextStyle = TextStyle::new(12.0, 580.0);
const BODY: TextStyle = TextStyle::new(13.0, 440.0);
const HEADING: TextStyle = TextStyle::new(15.0, 650.0);

pub(super) fn view(
    state: &State,
    p: &Palette,
    app: AppId,
    width: f32,
    height: f32,
    compact: bool,
) -> El {
    let maximized = state.desktop.maximized;
    let [x, y, w, h] = window_rect(width, height, compact, maximized);
    let radius = if maximized { 0.0 } else { 10.0 };
    let controls = row()
        .gap(if compact { 1.0 } else { 2.0 })
        .child(control(p, compact, Control::Minimize))
        .child(control(
            p,
            compact,
            if maximized {
                Control::Restore
            } else {
                Control::Maximize
            },
        ))
        .child(control(p, compact, Control::Close));
    let header = row()
        .w_full()
        .h(if compact { 50.0 } else { 36.0 })
        .pl(if compact { 10.0 } else { 13.0 })
        .pr(if compact { 3.0 } else { 8.0 })
        .gap(8.0)
        .center_y()
        .corners([radius, radius, 0.0, 0.0])
        .fill(p.surface_2)
        .child(icon(state, app.key(), 15.0).a11y_hidden())
        .child(
            text(app.label())
                .font(LABEL)
                .color(p.ink)
                .grow(1.0)
                .truncate()
                .a11y_hidden(),
        )
        .child(controls);
    let body = match app {
        AppId::Control => crate::ui::settings_body(state),
        AppId::Applications => applications(state, p, compact),
        AppId::Monitor => monitor(state, p, compact),
        AppId::Files | AppId::Backup | AppId::Logs => preview(state, p, app, compact),
    };
    let frame = col()
        .key("app-window")
        .role("dialog")
        .label(app.label())
        .absolute(x, y)
        .size(w, h)
        .min_h(0.0)
        .radius(radius)
        .fill(p.surface)
        .border(
            1.0,
            if maximized {
                p.line
            } else {
                p.accent.alpha(0.55)
            },
        )
        .on_background_press_tx(|_: &mut State, tx| tx.live_only())
        .child(header)
        .child(divider(p))
        .child(
            col()
                .key(format!("window-body-{}", app.key()))
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .corners([0.0, 0.0, radius, radius])
                .clip_rounded(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(body),
        );
    if maximized {
        frame
    } else {
        frame.shadow(17.0, 38.0, Color::BLACK.alpha(0.34))
    }
}

fn window_rect(width: f32, height: f32, compact: bool, maximized: bool) -> [f32; 4] {
    let work_height = (height - TASKBAR).max(0.0);
    if maximized {
        return [0.0, 0.0, width, work_height];
    }
    if compact {
        return [
            6.0,
            6.0,
            (width - 12.0).max(0.0),
            (work_height - 12.0).max(0.0),
        ];
    }
    let x = (width * 0.099).clamp(14.0, 142.0);
    let y = (work_height * 0.063).clamp(8.0, 54.0);
    [
        x,
        y,
        (width - x - 14.0).clamp(0.0, 740.0),
        (work_height - y - 12.0).clamp(0.0, 560.0),
    ]
}

#[derive(Clone, Copy)]
enum Control {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn control(p: &Palette, compact: bool, control: Control) -> El {
    let (key, label, action): (_, _, fn(&mut State)) = match control {
        Control::Minimize => ("window-minimize", "Minimize", State::minimize_app),
        Control::Maximize => ("window-maximize", "Maximize", State::toggle_app_maximized),
        Control::Restore => ("window-maximize", "Restore", State::toggle_app_maximized),
        Control::Close => ("window-close", "Close", State::close_app),
    };
    let mark = Shape::new();
    let c = p.muted;
    let mark = match control {
        Control::Minimize => mark.stroke([[2.0, 6.0], [10.0, 6.0]], 1.1, c),
        Control::Maximize => mark.stroke(
            [[2.5, 2.5], [9.5, 2.5], [9.5, 9.5], [2.5, 9.5], [2.5, 2.5]],
            1.0,
            c,
        ),
        Control::Restore => mark
            .stroke([[4.5, 2.0], [10.0, 2.0], [10.0, 7.5]], 1.0, c)
            .stroke(
                [[2.0, 4.5], [7.5, 4.5], [7.5, 10.0], [2.0, 10.0], [2.0, 4.5]],
                1.0,
                c,
            ),
        Control::Close => {
            mark.stroke([[2.5, 2.5], [9.5, 9.5]], 1.0, c)
                .stroke([[9.5, 2.5], [2.5, 9.5]], 1.0, c)
        }
    };
    let [w, h] = if compact { [44.0, 44.0] } else { [30.0, 26.0] };
    el().key(key)
        .role("button")
        .label(label)
        .focusable(true)
        .help(label)
        .size(w, h)
        .radius(5.0)
        .center()
        .hover(p.hover)
        .on_press(action)
        .child(el().size(12.0, 12.0).shape(mark))
}

fn divider(p: &Palette) -> El {
    el().w_full().h(1.0).fill(p.line.alpha(0.72))
}

fn copy(value: impl Into<String>, style: TextStyle, color: Color) -> El {
    text(value)
        .w_full()
        .wrap_anywhere()
        .font(style)
        .color(color)
}

fn card(p: &Palette, compact: bool) -> El {
    col()
        .w_full()
        .padding(if compact { 13.0 } else { 18.0 })
        .gap(8.0)
        .radius(8.0)
        .fill(p.surface_2)
        .border(1.0, p.line)
}

fn applications(state: &State, p: &Palette, compact: bool) -> El {
    let mut list = col()
        .w_full()
        .padding(if compact { 12.0 } else { 20.0 })
        .gap(8.0)
        .child(copy("Workspace applications", HEADING, p.ink))
        .child(
            copy(
                "Open a built-in app. Server-backed apps are previews in this build.",
                BODY,
                p.muted,
            )
            .mb(6.0),
        );
    for app in AppId::ALL
        .into_iter()
        .filter(|id| *id != AppId::Applications)
    {
        list = list.child(
            row()
                .key(format!("launch-{}", app.key()))
                .role("button")
                .label(app.label())
                .focusable(true)
                .on_press(move |state: &mut State| state.open_app(app))
                .w_full()
                .min_h(53.0)
                .padding(10.0)
                .gap(12.0)
                .center_y()
                .radius(7.0)
                .fill(p.surface_2)
                .border(1.0, p.line)
                .hover(p.hover)
                .child(icon(state, app.key(), 26.0).a11y_hidden())
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(2.0)
                        .child(copy(app.label(), LABEL, p.ink).a11y_hidden())
                        .child(copy(app.description(), BODY, p.muted)),
                ),
        );
    }
    list
}

fn preview(state: &State, p: &Palette, app: AppId, compact: bool) -> El {
    let (actions, columns, heading, detail): (&[&str], &[&str], _, _) = match app {
        AppId::Files => (
            &[
                "Upload",
                "New folder",
                "Download",
                "Rename",
                "Copy",
                "Move",
                "Delete",
            ],
            &["Name", "Size"],
            "File service not connected",
            "Browsing, uploading and downloading server files aren’t available in this build.",
        ),
        AppId::Backup => (
            &["Create backup", "Schedule", "Verify"],
            &["Archive", "Created", "Size"],
            "Backup service not connected",
            "This build can’t list, create, verify or restore server backups.",
        ),
        AppId::Logs => (
            &["Filter", "Export CSV"],
            &["Time", "Action", "Result"],
            "Activity logs not connected",
            "This build can’t read or export server audit logs.",
        ),
        _ => unreachable!("only server-backed previews are routed here"),
    };
    let pad = if compact { 8.0 } else { 10.0 };
    let mut toolbar = row().w_full().flex_wrap().center_y().gap(5.0);
    for (index, &action) in actions.iter().enumerate() {
        let fill = if index == 0 {
            p.accent.alpha(0.15)
        } else {
            p.surface
        };
        toolbar = toolbar.child(
            el().role("button")
                .label(action)
                .disabled(true)
                .h(28.0)
                .px(9.0)
                .center()
                .radius(5.0)
                .fill(fill)
                .border(1.0, p.line.alpha(0.68))
                .child(text(action).font(LABEL).color(p.muted).a11y_hidden()),
        );
    }
    let mut table_head = row()
        .w_full()
        .h(36.0)
        .px(pad)
        .gap(12.0)
        .center_y()
        .fill(p.surface_2);
    for (index, &header) in columns.iter().enumerate() {
        let name = text(header).font(LABEL).color(p.muted);
        table_head = table_head.child(if index == 0 {
            name.grow(1.0).truncate()
        } else {
            name.w(if compact { 56.0 } else { 90.0 }).align(Align::End)
        });
    }
    let notice = col()
        .w_full()
        .items_center()
        .gap(8.0)
        .px(if compact { 14.0 } else { 26.0 })
        .py(if compact { 20.0 } else { 34.0 })
        .child(icon(state, app.key(), 35.0).a11y_hidden())
        .child(copy(heading, HEADING, p.ink).align(Align::Center))
        .child(copy(detail, BODY, p.muted).align(Align::Center));
    col()
        .w_full()
        .child(col().w_full().padding(pad).fill(p.surface_2).child(toolbar))
        .child(divider(p))
        .child(
            row()
                .w_full()
                .h(38.0)
                .px(pad)
                .gap(6.0)
                .center_y()
                .child(
                    text(app.label())
                        .font(LABEL)
                        .color(p.ink)
                        .grow(1.0)
                        .truncate(),
                )
                .child(
                    el().h(22.0)
                        .px(8.0)
                        .center()
                        .radius(11.0)
                        .border(1.0, p.line)
                        .child(text("Preview").font(LABEL).color(p.muted)),
                ),
        )
        .child(divider(p))
        .child(table_head)
        .child(divider(p))
        .child(notice)
}

fn monitor(state: &State, p: &Palette, compact: bool) -> El {
    let (dot, headline, detail, retry) = monitor_status(state, p);
    let mut status = card(p, compact)
        .child(
            row()
                .w_full()
                .center_y()
                .gap(8.0)
                .child(el().size(8.0, 8.0).radius(4.0).fill(dot))
                .child(copy(headline, HEADING, p.ink).grow(1.0).min_w(0.0)),
        )
        .child(copy(detail, BODY, p.muted));
    if let Some(retry) = retry {
        status = status.child(copy(retry, LABEL, p.muted));
    }
    let action = match state.status.phase {
        Phase::Setup => Some(action_button(
            p,
            "monitor-open-settings",
            "Open Control Panel",
            State::open_settings,
        )),
        #[cfg(not(target_arch = "wasm32"))]
        Phase::Offline => Some(action_button(
            p,
            "reconnect",
            "Reconnect now",
            State::retry_now,
        )),
        _ => None,
    };
    if let Some(action) = action {
        status = status.child(row().w_full().child(action));
    }
    col()
        .w_full()
        .padding(if compact { 12.0 } else { 20.0 })
        .gap(10.0)
        .child(copy("Server connection", LABEL, p.muted))
        .child(status.live_region(LiveRegion::Polite))
        .child(
            card(p, compact)
                .child(copy("Host metrics unavailable", HEADING, p.ink))
                .child(copy(
                    "CPU, memory, storage and network metrics aren’t connected in this build.",
                    BODY,
                    p.muted,
                )),
        )
}

fn monitor_status(state: &State, p: &Palette) -> (Color, &'static str, String, Option<String>) {
    if state.status.phase == Phase::Setup {
        return (
            p.accent,
            "Server setup needed",
            state
                .status
                .error
                .clone()
                .unwrap_or_else(|| "Open Control Panel to enter the server address.".to_owned()),
            None,
        );
    }
    #[cfg(target_arch = "wasm32")]
    return (
        p.muted,
        "Connection status unavailable",
        format!(
            "Configured server: {}. This browser build doesn’t probe the server.",
            state.config.server
        ),
        None,
    );
    #[cfg(not(target_arch = "wasm32"))]
    match state.status.phase {
        Phase::Connecting => (
            p.accent,
            "Checking server reachability…",
            format!("Contacting {}.", state.config.server),
            None,
        ),
        Phase::Online => (
            super::rgb("#269465"),
            "Server reachable",
            format!("{} answered a connection check.", state.config.server),
            None,
        ),
        Phase::Offline => {
            let retry = if state.config.auto_reconnect {
                state.status.retry_in.map(|delay| {
                    format!(
                        "Retry scheduled after {}s (attempt {}).",
                        delay.as_secs().max(1),
                        state.status.attempts,
                    )
                })
            } else {
                Some("Automatic reconnection is off.".to_owned())
            };
            (
                super::rgb("#dc5060"),
                "Server unreachable",
                state
                    .status
                    .error
                    .clone()
                    .unwrap_or_else(|| "The server did not answer.".to_owned()),
                retry,
            )
        }
        Phase::Setup => unreachable!("setup is handled above"),
    }
}

fn action_button(
    p: &Palette,
    key: &'static str,
    label: &'static str,
    on_press: fn(&mut State),
) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .focusable(true)
        .center()
        .h(38.0)
        .px(12.0)
        .radius(6.0)
        .fill(p.accent)
        .hover(p.accent.alpha(0.89))
        .on_press(on_press)
        .child(text(label).font(LABEL).color(p.accent_ink).a11y_hidden())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_stays_above_the_taskbar_when_restored_or_maximized() {
        for (width, height, compact) in [
            (1440.0, 900.0, false),
            (720.0, 480.0, false),
            (320.0, 425.0, true),
        ] {
            for maximized in [true, false] {
                let [x, y, w, h] = window_rect(width, height, compact, maximized);
                assert!(x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0);
                assert!(x + w <= width && y + h <= height - TASKBAR);
                if maximized {
                    assert_eq!([x, y, w, h], [0.0, 0.0, width, height - TASKBAR]);
                }
            }
        }
    }
}
