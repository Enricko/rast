//! Classic desktop window chrome and the client-side application views.

use rune::{Color, El, ScrollSpec, Shape, TextStyle, col, el, row, text};

use super::{AppId, Palette, icon};
use crate::State;

const TASKBAR: f32 = 54.0;
const LABEL: TextStyle = TextStyle::new(12.0, 580.0);

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
    let body = crate::ui::settings_body(state);
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
        .child(el().w_full().h(1.0).fill(p.line))
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
