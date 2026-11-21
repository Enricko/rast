//! Window chrome, as the old RAST desktop drew it: the frame, the title bar that drags the
//! window, its buttons, the resize handles around it and the snap preview. Where windows sit
//! and how gestures move them lives in `windows.rs`.

use rune::{Color, Cursor, CursorImage, El, Shape, TextStyle, col, el, row, text};

use super::windows::{self, Area, Edge, Rect, Window};
use super::{AppId, Palette, icon, rgb};
use crate::State;

/// The title bar's height: the old RAST's 40 points, 44 for a finger on a phone.
const TITLE_H: f32 = 40.0;
const TITLE_H_PHONE: f32 = 44.0;
const TITLE: TextStyle = TextStyle::new(12.5, 600.0);
const RADIUS: f32 = 10.0;
/// The resize strips sit outside the frame, so the content keeps every point: edges this
/// thick, starting this far from the corners, and corners this big.
const EDGE: f32 = 6.0;
const EDGE_INSET: f32 = 8.0;
const CORNER: f32 = 12.0;
const CORNER_OUT: f32 = 8.0;
/// Content narrower than this lays out as on a phone.
const NARROW: f32 = 680.0;

/// CSS's diagonal resize arrows, which Rune's cursors lack.
const NWSE: CursorImage = CursorImage::png(
    include_bytes!("../../assets/cursor/resize-nwse.png"),
    [12.0, 12.0],
);
const NESW: CursorImage = CursorImage::png(
    include_bytes!("../../assets/cursor/resize-nesw.png"),
    [12.0, 12.0],
);

/// The window's title: the app, and the page it shows.
pub(super) fn title(state: &State, app: AppId) -> String {
    match app {
        AppId::Control => format!("{} — {}", app.label(), super::control::page_title(state)),
        #[cfg(test)]
        AppId::Probe => app.label().to_owned(),
    }
}

/// Every window on screen, back to front, with the snap preview under the one being dragged.
pub(super) fn view(state: &State, p: &Palette, area: Area) -> Vec<El> {
    let wm = &state.desktop.windows;
    let phone = windows::fills(area);
    let focused = wm.focused();
    let mut layers = Vec::new();
    for window in wm.stacked() {
        // A phone shows only the window in front, as an app switcher does.
        if phone && focused != Some(window.app) {
            continue;
        }
        if let Some(zone) = wm
            .grab
            .filter(|grab| grab.app == window.app)
            .and_then(|grab| grab.zone)
        {
            layers.push(preview(p, zone.rect(area)));
        }
        layers.push(frame(state, p, window, area, focused == Some(window.app)));
        if window.floats(area) {
            layers.extend(handles(window, area));
        }
    }
    layers
}

fn frame(state: &State, p: &Palette, window: &Window, area: Area, focused: bool) -> El {
    let app = window.app;
    let phone = windows::fills(area);
    let tiled = !phone && !window.maximized && window.snapped.is_some();
    let square = phone || window.maximized || tiled;
    let radius = if square { 0.0 } else { RADIUS };
    let [x, y, w, h] = window.frame(area);
    let title = title(state, app);

    let mut buttons = row()
        .gap(1.0)
        .child(button(p, app, Button::Minimize, phone));
    if !phone {
        buttons = buttons.child(button(
            p,
            app,
            if window.maximized {
                Button::Restore
            } else {
                Button::Maximize
            },
            phone,
        ));
    }
    buttons = buttons.child(button(p, app, Button::Close, phone));
    let header = row()
        .key(format!("window-{}-title", app.key()))
        .w_full()
        .h(if phone { TITLE_H_PHONE } else { TITLE_H })
        .pl(12.0)
        .pr(6.0)
        .gap(8.0)
        .center_y()
        .corners([radius, radius, 0.0, 0.0])
        .fill(p.surface_2)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(icon(state, app.key(), 16.0).a11y_hidden())
        .child(
            text(title.clone())
                .font(TITLE)
                .color(p.ink)
                .grow(1.0)
                .min_w(0.0)
                .truncate()
                .a11y_hidden(),
        )
        .child(buttons)
        .on_drag(move |s: &mut State, d, _| windows::drag_title(s, d, app, area))
        .on_double_press(move |s: &mut State| {
            if !windows::fills(area) {
                s.toggle_window_maximized(app);
            }
        });

    let body = match app {
        AppId::Control => super::control::view(state, p, phone || w < NARROW),
        #[cfg(test)]
        AppId::Probe => el().w_full().grow(1.0),
    };
    let mut content = col()
        .key(format!("window-{}-body", app.key()))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .corners([0.0, 0.0, radius, radius])
        .clip_rounded(0.0)
        .child(body);
    if !focused {
        // A press anywhere in a window behind brings it to the front first.
        content = content.child(
            el().key(format!("window-{}-raise", app.key()))
                .label(format!("Bring {} to the front", app.label()))
                .focusable(false)
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .on_press(move |s: &mut State| s.focus_window(app)),
        );
    }

    let ring = if focused {
        p.line.mix(p.accent, 0.45)
    } else {
        p.line
    };
    let mut frame = col()
        .key(format!("window-{}", app.key()))
        .role("dialog")
        .label(title)
        .absolute(x, y)
        .size(w, h)
        .min_h(0.0)
        .radius(radius)
        .fill(p.surface)
        .on_background_press_tx(move |s: &mut State, tx| {
            if s.desktop.active() == Some(app) {
                tx.live_only();
            } else {
                s.focus_window(app);
            }
        })
        .child(header)
        .child(content);
    frame = if phone {
        frame
    } else if tiled {
        frame.border_sides([0.0, 1.0, 0.0, 1.0], ring)
    } else {
        frame.border(1.0, ring)
    };
    if square {
        return frame;
    }
    let dark = p.dark;
    match (dark, focused) {
        (false, false) => frame.shadow(16.0, 40.0, rgb("#101822").alpha(0.18)),
        (false, true) => frame.shadow(18.0, 44.0, rgb("#121a24").alpha(0.22)),
        (true, false) => frame.shadow(18.0, 44.0, Color::BLACK.alpha(0.55)),
        (true, true) => frame.shadow(20.0, 48.0, Color::BLACK.alpha(0.6)),
    }
}

/// Where dropping the dragged title would put the window: an accent outline, inset 6.
fn preview(p: &Palette, [x, y, w, h]: Rect) -> El {
    el().key("window-snap-preview")
        .a11y_hidden()
        .absolute(x + 6.0, y + 6.0)
        .size((w - 12.0).max(80.0), (h - 12.0).max(80.0))
        .radius(RADIUS)
        .fill(p.accent.alpha(0.14))
        .border(1.0, p.accent.alpha(0.7))
}

/// The eight strips around a floating window that resize it.
fn handles(window: &Window, area: Area) -> Vec<El> {
    let app = window.app;
    let [x, y, w, h] = window.frame(area);
    Edge::ALL
        .into_iter()
        .map(|edge| {
            let (rect, cursor) = match edge {
                Edge::N => (
                    [x + EDGE_INSET, y - EDGE, w - 2.0 * EDGE_INSET, EDGE],
                    Cursor::ResizeRow,
                ),
                Edge::S => (
                    [x + EDGE_INSET, y + h, w - 2.0 * EDGE_INSET, EDGE],
                    Cursor::ResizeRow,
                ),
                Edge::E => (
                    [x + w, y + EDGE_INSET, EDGE, h - 2.0 * EDGE_INSET],
                    Cursor::ResizeColumn,
                ),
                Edge::W => (
                    [x - EDGE, y + EDGE_INSET, EDGE, h - 2.0 * EDGE_INSET],
                    Cursor::ResizeColumn,
                ),
                Edge::NE => (
                    [x + w + CORNER_OUT - CORNER, y - CORNER_OUT, CORNER, CORNER],
                    Cursor::Image(NESW),
                ),
                Edge::NW => (
                    [x - CORNER_OUT, y - CORNER_OUT, CORNER, CORNER],
                    Cursor::Image(NWSE),
                ),
                Edge::SE => (
                    [
                        x + w + CORNER_OUT - CORNER,
                        y + h + CORNER_OUT - CORNER,
                        CORNER,
                        CORNER,
                    ],
                    Cursor::Image(NWSE),
                ),
                Edge::SW => (
                    [x - CORNER_OUT, y + h + CORNER_OUT - CORNER, CORNER, CORNER],
                    Cursor::Image(NESW),
                ),
            };
            let [ex, ey, ew, eh] = rect;
            el().key(format!("window-{}-edge-{}", app.key(), edge.name()))
                .a11y_hidden()
                .absolute(ex, ey)
                .size(ew.max(0.0), eh.max(0.0))
                .cursor(cursor)
                .on_drag(move |s: &mut State, d, _| windows::drag_edge(s, d, app, edge, area))
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Button {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn button(p: &Palette, app: AppId, button: Button, phone: bool) -> El {
    let (name, label) = match button {
        Button::Minimize => ("minimize", "Minimize"),
        Button::Maximize => ("maximize", "Maximize"),
        Button::Restore => ("maximize", "Restore"),
        Button::Close => ("close", "Close"),
    };
    let c = p.muted;
    let mark = Shape::new();
    let mark = match button {
        Button::Minimize => mark.stroke([[2.0, 7.0], [12.0, 7.0]], 1.2, c),
        Button::Maximize => mark.stroke(
            [
                [2.5, 2.5],
                [11.5, 2.5],
                [11.5, 11.5],
                [2.5, 11.5],
                [2.5, 2.5],
            ],
            1.1,
            c,
        ),
        Button::Restore => mark
            .stroke([[5.0, 2.5], [11.5, 2.5], [11.5, 9.0]], 1.1, c)
            .stroke(
                [[2.5, 5.0], [9.0, 5.0], [9.0, 11.5], [2.5, 11.5], [2.5, 5.0]],
                1.1,
                c,
            ),
        Button::Close => mark.stroke([[3.0, 3.0], [11.0, 11.0]], 1.2, c).stroke(
            [[11.0, 3.0], [3.0, 11.0]],
            1.2,
            c,
        ),
    };
    let [w, h] = if phone { [44.0, 44.0] } else { [30.0, 24.0] };
    let item = el()
        .key(format!("window-{}-{name}", app.key()))
        .role("button")
        .label(label)
        .focusable(true)
        .help(label)
        .size(w, h)
        .radius(5.0)
        .center()
        .child(el().size(14.0, 14.0).shape(mark).group_color(Color::WHITE));
    let item = if matches!(button, Button::Close) {
        item.hover_group().hover(p.danger)
    } else {
        item.hover(p.hover)
    };
    item.on_press(move |s: &mut State| match button {
        Button::Minimize => s.minimize_window(app),
        Button::Maximize | Button::Restore => s.toggle_window_maximized(app),
        Button::Close => s.close_window(app),
    })
}

#[cfg(test)]
mod tests {
    use rune::{Env, Runtime};

    use super::super::{AppId, BAR};
    use crate::{Config, SIZE, Screen, State, ui};

    fn desktop() -> Runtime<State> {
        let mut state = State::new(Config::default());
        state.signed_in = true;
        state.screen = Screen::Desktop;
        state.persist = false;
        state.open_app(AppId::Control);
        let env = Env {
            system_dark: true,
            scale: 2.0,
        };
        let mut rt = Runtime::new(state, ui::view, SIZE, env);
        settle(&mut rt);
        rt
    }

    /// Long enough for a maximize or snap to finish animating.
    fn settle(rt: &mut Runtime<State>) {
        for _ in 0..90 {
            rt.frame(1.0 / 60.0);
        }
    }

    fn rect(rt: &Runtime<State>, key: &str) -> [f32; 4] {
        rt.presented(key)
            .unwrap_or_else(|| panic!("`{key}` is on screen"))
            .0
    }

    fn shown(rt: &Runtime<State>, key: &str) -> bool {
        rt.find(key).is_some()
    }

    fn press(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt
            .find(key)
            .unwrap_or_else(|| panic!("`{key}` is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        settle(rt);
    }

    fn drag(rt: &mut Runtime<State>, from: [f32; 2], by: [f32; 2]) {
        rt.pointer_down(from[0], from[1]);
        rt.frame(1.0 / 60.0);
        for step in 1..=4 {
            let t = step as f32 / 4.0;
            rt.pointer_move(from[0] + by[0] * t, from[1] + by[1] * t);
            rt.frame(1.0 / 60.0);
        }
        rt.pointer_up(from[0] + by[0], from[1] + by[1]);
        settle(rt);
    }

    #[test]
    fn the_title_drags_the_window_and_a_double_click_maximizes_it() {
        let mut rt = desktop();
        assert_eq!(rect(&rt, "window-controlpanel"), [8.0, 8.0, 1060.0, 700.0]);
        drag(&mut rt, [300.0, 28.0], [120.0, 40.0]);
        assert_eq!(
            rect(&rt, "window-controlpanel")[..2],
            [128.0, 48.0],
            "the window followed the pointer"
        );
        assert!(
            shown(&rt, "window-controlpanel-edge-se"),
            "a floating window has resize handles"
        );

        let [x, y, ..] = rect(&rt, "window-controlpanel-title");
        for _ in 0..2 {
            rt.pointer_down(x + 200.0, y + 20.0);
            rt.pointer_up(x + 200.0, y + 20.0);
            rt.frame(1.0 / 60.0);
        }
        settle(&mut rt);
        assert_eq!(
            rect(&rt, "window-controlpanel"),
            [0.0, 0.0, SIZE.0, SIZE.1 - BAR],
            "a double click on the title maximizes"
        );
        assert!(
            !shown(&rt, "window-controlpanel-edge-se"),
            "a maximized window doesn't resize"
        );
    }

    #[test]
    fn dropping_the_title_on_the_left_edge_snaps_the_window_to_that_half() {
        let mut rt = desktop();
        drag(&mut rt, [300.0, 28.0], [-296.0, 200.0]);
        assert_eq!(
            rect(&rt, "window-controlpanel"),
            [0.0, 0.0, SIZE.0 / 2.0, SIZE.1 - BAR]
        );
        drag(&mut rt, [300.0, 20.0], [100.0, 100.0]);
        assert_eq!(
            rect(&rt, "window-controlpanel")[2..],
            [1060.0, 700.0],
            "dragging it away brings back its own size"
        );
    }

    #[test]
    fn the_corner_resizes_and_the_buttons_minimize_restore_and_close() {
        let mut rt = desktop();
        let [x, y, w, h] = rect(&rt, "window-controlpanel-edge-se");
        drag(&mut rt, [x + w / 2.0, y + h / 2.0], [-200.0, -100.0]);
        assert_eq!(rect(&rt, "window-controlpanel")[2..], [860.0, 600.0]);

        press(&mut rt, "window-controlpanel-minimize");
        assert!(!shown(&rt, "window-controlpanel"), "minimized");
        assert_eq!(rt.state.screen, Screen::Desktop);
        press(&mut rt, "task-controlpanel");
        assert_eq!(
            rect(&rt, "window-controlpanel")[2..],
            [860.0, 600.0],
            "the taskbar brings it back at the same size"
        );
        assert_eq!(rt.state.screen, Screen::Settings);
        press(&mut rt, "task-controlpanel");
        assert!(
            !shown(&rt, "window-controlpanel"),
            "the taskbar minimizes the window in front"
        );
        press(&mut rt, "task-controlpanel");
        press(&mut rt, "window-controlpanel-close");
        assert!(!shown(&rt, "window-controlpanel"));
        assert!(!shown(&rt, "task-controlpanel"), "closed");
        assert_eq!(rt.state.screen, Screen::Desktop);
    }

    #[test]
    fn a_phone_fills_the_screen_with_the_window() {
        let mut rt = desktop();
        rt.set_viewport((390.0, 844.0), 3.0);
        settle(&mut rt);
        assert_eq!(
            rect(&rt, "window-controlpanel"),
            [0.0, 0.0, 390.0, 844.0 - BAR]
        );
        assert!(!shown(&rt, "window-controlpanel-maximize"));
        assert!(!shown(&rt, "window-controlpanel-edge-se"));
    }
}
