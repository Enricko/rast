//! The window manager: where each open window sits, which one is in front, and the gestures
//! that move, resize, snap and maximize windows, as the old RAST desktop did.
//!
//! Positions are in work-area points: the desktop above the taskbar, origin top-left.

use rune::{Drag, Phase};

use super::AppId;
use crate::{Screen, State};

/// A window's place: x, y, width, height.
pub(crate) type Rect = [f32; 4];
/// The work area's width and height.
pub(crate) type Area = [f32; 2];

/// Each window opens this much further down and right than the one before, for seven steps.
const CASCADE: f32 = 26.0;
/// Windows fill a work area this narrow (or lower than this), as apps do on a phone.
const PHONE: Area = [820.0, 420.0];
/// A title dragged this close to the top, left or right edge snaps its window there.
const SNAP_EDGE: f32 = 18.0;
/// A press travels this far before it moves a window, so a click on a title stays a click.
const MOVE_SLOP: f32 = 4.0;
/// However far a window goes, this much of it stays on screen to grab it back by.
const KEEP_LEFT: f32 = 120.0;
const KEEP_RIGHT: f32 = 80.0;
const KEEP_BOTTOM: f32 = 60.0;

/// Whether windows fill the work area (a phone): there they neither move nor resize.
pub(crate) fn fills(area: Area) -> bool {
    area[0] <= PHONE[0] || area[1] < PHONE[1]
}

/// The half of the screen a snapped window covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
}

/// Where a title dragged to an edge would put its window when dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Zone {
    /// The top edge: maximized.
    Top,
    Half(Side),
}

impl Zone {
    pub(crate) fn rect(self, area: Area) -> Rect {
        match self {
            Self::Top => [0.0, 0.0, area[0], area[1]],
            Self::Half(side) => half(side, area),
        }
    }
}

fn half(side: Side, [w, h]: Area) -> Rect {
    let left = (w / 2.0).round();
    match side {
        Side::Left => [0.0, 0.0, left, h],
        Side::Right => [left, 0.0, w - left, h],
    }
}

/// The edge or corner a resize handle drags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edge {
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Edge {
    pub(crate) const ALL: [Self; 8] = [
        Self::N,
        Self::S,
        Self::E,
        Self::W,
        Self::NE,
        Self::NW,
        Self::SE,
        Self::SW,
    ];
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::N => "n",
            Self::S => "s",
            Self::E => "e",
            Self::W => "w",
            Self::NE => "ne",
            Self::NW => "nw",
            Self::SE => "se",
            Self::SW => "sw",
        }
    }
    const fn north(self) -> bool {
        matches!(self, Self::N | Self::NE | Self::NW)
    }
    const fn south(self) -> bool {
        matches!(self, Self::S | Self::SE | Self::SW)
    }
    const fn east(self) -> bool {
        matches!(self, Self::E | Self::NE | Self::SE)
    }
    const fn west(self) -> bool {
        matches!(self, Self::W | Self::NW | Self::SW)
    }
}

/// One open window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Window {
    pub(crate) app: AppId,
    /// Where it sits when neither maximized nor snapped. `None` until it is first moved or
    /// resized: it opens at its app's size in its cascade step, whatever the screen size.
    rect: Option<Rect>,
    cascade: u32,
    pub(crate) maximized: bool,
    pub(crate) snapped: Option<Side>,
    pub(crate) minimized: bool,
    /// Stacking order: the highest is in front.
    z: u32,
}

impl Window {
    fn new(app: AppId, cascade: u32) -> Self {
        Self {
            app,
            rect: None,
            cascade,
            maximized: false,
            snapped: None,
            minimized: false,
            z: 0,
        }
    }

    /// Where the window shows in a work area of `area`.
    pub(crate) fn frame(&self, area: Area) -> Rect {
        if fills(area) || self.maximized {
            return [0.0, 0.0, area[0], area[1]];
        }
        if let Some(side) = self.snapped {
            return half(side, area);
        }
        self.floating_frame(area)
    }

    /// The window's own place, ignoring maximized and snapped.
    fn floating_frame(&self, area: Area) -> Rect {
        let rect = self
            .rect
            .unwrap_or_else(|| opening(area, self.cascade, self.app.open_size()));
        placed(rect, area, self.app.min_size())
    }

    /// Whether its edges resize it and its title moves it freely.
    pub(crate) fn floats(&self, area: Area) -> bool {
        !fills(area) && !self.maximized && self.snapped.is_none()
    }
}

/// A new window's place: its size where that fits, stepped down and right from the top-left
/// corner for each window opened before it.
fn opening(area: Area, cascade: u32, [w, h]: [f32; 2]) -> Rect {
    let [aw, ah] = area;
    let w = w.min(aw - 16.0);
    let h = h.min(ah - 16.0);
    let step = (cascade % 7) as f32 * CASCADE;
    let x = (8.0 + step).min((aw - w - 8.0).max(8.0));
    let y = (8.0 + step).min((ah - h - 8.0).max(8.0));
    [x, y, w, h]
}

/// `rect` kept within the work area: no smaller than `min`, no bigger than the area, and with
/// enough of its title on screen to drag it back by.
fn placed([x, y, w, h]: Rect, [aw, ah]: Area, [min_w, min_h]: [f32; 2]) -> Rect {
    let w = w.max(min_w).min(aw);
    let h = h.max(min_h).min(ah);
    [
        x.max(KEEP_LEFT - w).min((aw - KEEP_RIGHT).max(0.0)),
        y.max(0.0).min((ah - KEEP_BOTTOM).max(0.0)),
        w,
        h,
    ]
}

/// The snap zone under the pointer while a title is dragged.
fn zone_at([x, y]: [f32; 2], [aw, _]: Area) -> Option<Zone> {
    if y <= SNAP_EDGE {
        Some(Zone::Top)
    } else if x <= SNAP_EDGE {
        Some(Zone::Half(Side::Left))
    } else if x >= aw - SNAP_EDGE {
        Some(Zone::Half(Side::Right))
    } else {
        None
    }
}

/// A move (`edge: None`) or resize in progress.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Grab {
    pub(crate) app: AppId,
    edge: Option<Edge>,
    /// The window as it was, put back if the gesture is cancelled (Escape).
    before: Window,
    /// Its frame when the gesture started; the pointer's travel is added to it.
    start: Rect,
    moved: bool,
    /// Where dropping the title now would snap the window.
    pub(crate) zone: Option<Zone>,
}

/// The open windows.
#[derive(Default)]
pub(crate) struct Windows {
    /// In the order they opened, which the taskbar keeps.
    list: Vec<Window>,
    /// The window in front that takes the keyboard; `None` when every window is minimized.
    focused: Option<AppId>,
    /// The highest stacking order handed out so far.
    top: u32,
    /// How many windows have opened, which steps each new one down the cascade.
    opened: u32,
    pub(crate) grab: Option<Grab>,
}

impl Windows {
    pub(crate) fn focused(&self) -> Option<AppId> {
        self.focused
    }

    pub(crate) fn get(&self, app: AppId) -> Option<&Window> {
        self.list.iter().find(|w| w.app == app)
    }

    fn get_mut(&mut self, app: AppId) -> Option<&mut Window> {
        self.list.iter_mut().find(|w| w.app == app)
    }

    /// The windows in taskbar order.
    pub(crate) fn all(&self) -> &[Window] {
        &self.list
    }

    /// The windows on screen, back to front.
    pub(crate) fn stacked(&self) -> Vec<&Window> {
        let mut shown: Vec<_> = self.list.iter().filter(|w| !w.minimized).collect();
        shown.sort_by_key(|w| w.z);
        shown
    }

    /// Open `app`'s window, or bring it back if it is already open. True when it is new.
    fn open(&mut self, app: AppId) -> bool {
        let new = self.get(app).is_none();
        if new {
            self.list.push(Window::new(app, self.opened));
            self.opened += 1;
        }
        self.focus(app);
        new
    }

    /// Put `app`'s window in front, restoring it if minimized.
    fn focus(&mut self, app: AppId) {
        self.top += 1;
        let z = self.top;
        if let Some(window) = self.get_mut(app) {
            window.minimized = false;
            window.z = z;
            self.focused = Some(app);
        }
    }

    fn minimize(&mut self, app: AppId) {
        if let Some(window) = self.get_mut(app) {
            window.minimized = true;
        }
        if self.focused == Some(app) {
            self.focus_front();
        }
    }

    fn close(&mut self, app: AppId) {
        self.list.retain(|w| w.app != app);
        if self.grab.is_some_and(|g| g.app == app) {
            self.grab = None;
        }
        if self.focused == Some(app) {
            self.focus_front();
        }
    }

    /// Give the focus to the front window still on screen, if any.
    fn focus_front(&mut self) {
        self.focused = self
            .list
            .iter()
            .filter(|w| !w.minimized)
            .max_by_key(|w| w.z)
            .map(|w| w.app);
    }

    fn toggle_maximized(&mut self, app: AppId) {
        if let Some(window) = self.get_mut(app) {
            window.maximized = !window.maximized;
            window.snapped = None;
        }
        self.focus(app);
    }

    /// A taskbar button: restore a minimized window, minimize the focused one, and bring any
    /// other to the front.
    fn press_task(&mut self, app: AppId) {
        match self.get(app) {
            Some(w) if !w.minimized && self.focused == Some(app) => self.minimize(app),
            Some(_) => self.focus(app),
            None => {}
        }
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    fn start(&mut self, app: AppId, edge: Option<Edge>, area: Area) {
        if let Some(window) = self.get(app).copied() {
            self.grab = Some(Grab {
                app,
                edge,
                before: window,
                start: window.frame(area),
                moved: false,
                zone: None,
            });
        }
    }

    /// The pointer moved `travel` since the press on `app`'s title, and is now at `pointer`.
    fn move_to(&mut self, app: AppId, travel: [f32; 2], pointer: [f32; 2], area: Area) {
        let Some(mut grab) = self.grab.filter(|g| g.app == app && g.edge.is_none()) else {
            return;
        };
        let [dx, dy] = travel;
        if !grab.moved && dx.abs() + dy.abs() < MOVE_SLOP {
            return;
        }
        let Some(window) = self.get_mut(app) else {
            return;
        };
        if !grab.moved && (window.maximized || window.snapped.is_some()) {
            // Tear a maximized or snapped window off at its own size, keeping the point of
            // the title that was pressed under the pointer.
            let [_, _, w, h] = window.floating_frame(area);
            let [sx, sy, sw, _] = grab.start;
            let pressed = pointer[0] - dx;
            let along = ((pressed - sx) / sw.max(1.0)).clamp(0.0, 1.0);
            grab.start = [pressed - along * w, sy, w, h];
            window.maximized = false;
            window.snapped = None;
        }
        grab.moved = true;
        // While a snap shows, the window holds still under it.
        grab.zone = zone_at(pointer, area);
        if grab.zone.is_none() {
            let [sx, sy, w, h] = grab.start;
            window.rect = Some(placed(
                [sx + dx, sy + dy, w, h],
                area,
                window.app.min_size(),
            ));
        }
        self.grab = Some(grab);
    }

    /// The pointer moved `travel` since the press on one of `app`'s edges.
    fn resize_to(&mut self, app: AppId, travel: [f32; 2]) {
        let Some(mut grab) = self.grab.filter(|g| g.app == app) else {
            return;
        };
        let Some(edge) = grab.edge else {
            return;
        };
        let Some(window) = self.get_mut(app) else {
            return;
        };
        let [dx, dy] = travel;
        let [min_w, min_h] = window.app.min_size();
        let [x0, y0, w0, h0] = grab.start;
        let [mut x, mut y, mut w, mut h] = grab.start;
        if edge.east() {
            w = (w0 + dx).max(min_w);
        }
        if edge.west() {
            w = (w0 - dx).max(min_w);
            x = x0 + w0 - w;
        }
        if edge.south() {
            h = (h0 + dy).max(min_h);
        }
        if edge.north() {
            // The top edge stops at the top of the screen, where the title would be lost.
            y = (y0 + dy).min(y0 + h0 - min_h).max(0.0);
            h = y0 + h0 - y;
        }
        // A snapped window floats at its new size once resized.
        window.snapped = None;
        window.rect = Some([x, y, w, h]);
        grab.moved = true;
        self.grab = Some(grab);
    }

    /// The press on `app`'s title or edge came up, or was cancelled.
    fn finish(&mut self, app: AppId, cancelled: bool) {
        let Some(grab) = self.grab.take().filter(|g| g.app == app) else {
            return;
        };
        let Some(window) = self.get_mut(app) else {
            return;
        };
        if cancelled {
            *window = Window {
                z: window.z,
                minimized: window.minimized,
                ..grab.before
            };
            return;
        }
        match grab.zone {
            Some(Zone::Top) => {
                window.maximized = true;
                window.snapped = None;
            }
            Some(Zone::Half(side)) => {
                window.maximized = false;
                window.snapped = Some(side);
            }
            None => {}
        }
    }
}

impl State {
    /// Open `app`'s window, or bring it to the front.
    pub(crate) fn open_app(&mut self, app: AppId) {
        self.dismiss_panel();
        if self.desktop.windows.open(app) && app == AppId::Control {
            self.server_field = rune::TextEdit::new(self.config.server.clone());
            self.settings_error = None;
            self.reset_control();
        }
        self.sync_screen();
    }

    pub(crate) fn focus_window(&mut self, app: AppId) {
        self.desktop.windows.focus(app);
        self.sync_screen();
    }

    pub(crate) fn minimize_window(&mut self, app: AppId) {
        self.desktop.windows.minimize(app);
        self.sync_screen();
    }

    pub(crate) fn close_window(&mut self, app: AppId) {
        if app == AppId::Control
            && self.control.tab == super::control::Tab::Appearance
            && !self.desktop.look.dialog
        {
            // An unsaved preview leaves with its page.
            self.discard_appearance();
        }
        self.desktop.windows.close(app);
        self.sync_screen();
    }

    /// Every window out of the way (the desktop menu's *Show desktop*).
    pub(crate) fn show_desktop(&mut self) {
        self.desktop.menu = None;
        let apps: Vec<AppId> = self.desktop.windows.all().iter().map(|w| w.app).collect();
        for app in apps {
            self.desktop.windows.minimize(app);
        }
        self.sync_screen();
    }

    pub(crate) fn toggle_window_maximized(&mut self, app: AppId) {
        self.desktop.windows.toggle_maximized(app);
        self.sync_screen();
    }

    /// A press on `app`'s taskbar button.
    pub(crate) fn press_task(&mut self, app: AppId) {
        self.dismiss_panel();
        self.desktop.windows.press_task(app);
        self.sync_screen();
    }

    /// Close the focused window.
    pub(crate) fn close_app(&mut self) {
        if let Some(app) = self.desktop.active() {
            self.close_window(app);
        }
    }

    /// Maximize the focused window, or restore it.
    pub(crate) fn toggle_app_maximized(&mut self) {
        if let Some(app) = self.desktop.active() {
            self.toggle_window_maximized(app);
        }
    }

    /// The signed-in screens follow the focused window: Control Panel in front is Settings.
    fn sync_screen(&mut self) {
        if matches!(self.screen, Screen::Desktop | Screen::Settings) {
            self.screen = if self.desktop.active() == Some(AppId::Control) {
                Screen::Settings
            } else {
                Screen::Desktop
            };
        }
    }
}

/// The title bar's drag: Down brings the window forward, Move drags it (tearing a maximized
/// or snapped one off) and previews a snap at the edges, and Up drops it.
pub(crate) fn drag_title(state: &mut State, d: &Drag, app: AppId, area: Area) {
    match d.phase {
        Phase::Down => {
            state.focus_window(app);
            if !fills(area) {
                state.desktop.windows.start(app, None, area);
            }
        }
        Phase::Move => state
            .desktop
            .windows
            .move_to(app, [d.dx, d.dy], d.window_point(), area),
        Phase::Up => state.desktop.windows.finish(app, d.cancelled()),
    }
}

/// A resize handle's drag on `edge`.
pub(crate) fn drag_edge(state: &mut State, d: &Drag, app: AppId, edge: Edge, area: Area) {
    match d.phase {
        Phase::Down => {
            state.focus_window(app);
            state.desktop.windows.start(app, Some(edge), area);
        }
        Phase::Move => state.desktop.windows.resize_to(app, [d.dx, d.dy]),
        Phase::Up => state.desktop.windows.finish(app, d.cancelled()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Area = [1280.0, 756.0];

    fn opened(apps: &[AppId]) -> Windows {
        let mut windows = Windows::default();
        for &app in apps {
            windows.open(app);
        }
        windows
    }

    #[test]
    fn a_window_opens_at_the_old_size_and_each_next_one_cascades() {
        let windows = opened(&[AppId::Control, AppId::Probe]);
        let first = windows.get(AppId::Control).unwrap().frame(AREA);
        let second = windows.get(AppId::Probe).unwrap().frame(AREA);
        assert_eq!(first, [8.0, 8.0, 1060.0, 700.0], "Control Panel's own size");
        assert_eq!(second, [34.0, 34.0, 940.0, 620.0]);
        assert_eq!(
            windows.focused(),
            Some(AppId::Probe),
            "the newest is in front"
        );
        let order: Vec<_> = windows.stacked().iter().map(|w| w.app).collect();
        assert_eq!(order, [AppId::Control, AppId::Probe]);
    }

    #[test]
    fn a_window_fits_a_small_screen_and_fills_a_phone() {
        let windows = opened(&[AppId::Control]);
        let window = windows.get(AppId::Control).unwrap();
        let [x, y, w, h] = window.frame([900.0, 560.0]);
        assert!(x >= 0.0 && y >= 0.0 && x + w <= 900.0 && y + h <= 560.0);
        assert_eq!(window.frame([820.0, 800.0]), [0.0, 0.0, 820.0, 800.0]);
        assert_eq!(window.frame([390.0, 800.0]), [0.0, 0.0, 390.0, 800.0]);
        assert!(!window.floats([390.0, 800.0]));
    }

    #[test]
    fn dragging_the_title_moves_the_window_and_escape_puts_it_back() {
        let mut windows = opened(&[AppId::Control]);
        windows.start(AppId::Control, None, AREA);
        windows.move_to(AppId::Control, [2.0, 1.0], [100.0, 20.0], AREA);
        assert_eq!(
            windows.get(AppId::Control).unwrap().frame(AREA)[..2],
            [8.0, 8.0],
            "a press that barely moves is still a click"
        );
        windows.move_to(AppId::Control, [200.0, 90.0], [300.0, 110.0], AREA);
        assert_eq!(
            windows.get(AppId::Control).unwrap().frame(AREA),
            [208.0, 98.0, 1060.0, 700.0]
        );
        windows.finish(AppId::Control, true);
        assert_eq!(
            windows.get(AppId::Control).unwrap().frame(AREA),
            [8.0, 8.0, 1060.0, 700.0]
        );
    }

    #[test]
    fn a_window_keeps_its_title_on_screen() {
        let mut windows = opened(&[AppId::Control]);
        windows.start(AppId::Control, None, AREA);
        windows.move_to(AppId::Control, [-5000.0, -900.0], [100.0, 300.0], AREA);
        let [x, y, w, _] = windows.get(AppId::Control).unwrap().frame(AREA);
        assert_eq!([x + w, y], [KEEP_LEFT, 0.0]);
        windows.move_to(AppId::Control, [5000.0, 5000.0], [100.0, 300.0], AREA);
        let [x, y, ..] = windows.get(AppId::Control).unwrap().frame(AREA);
        assert_eq!([x, y], [AREA[0] - KEEP_RIGHT, AREA[1] - KEEP_BOTTOM]);
    }

    #[test]
    fn dropping_a_title_on_an_edge_snaps_or_maximizes() {
        let mut windows = opened(&[AppId::Control]);
        windows.start(AppId::Control, None, AREA);
        windows.move_to(AppId::Control, [-90.0, 40.0], [4.0, 300.0], AREA);
        assert_eq!(windows.grab.unwrap().zone, Some(Zone::Half(Side::Left)));
        assert_eq!(
            windows.get(AppId::Control).unwrap().frame(AREA)[..2],
            [8.0, 8.0],
            "the window holds still while the snap shows"
        );
        windows.finish(AppId::Control, false);
        let window = *windows.get(AppId::Control).unwrap();
        assert_eq!(window.frame(AREA), [0.0, 0.0, 640.0, AREA[1]]);
        assert!(!window.floats(AREA));

        windows.start(AppId::Control, None, AREA);
        windows.move_to(AppId::Control, [600.0, -280.0], [900.0, 2.0], AREA);
        assert_eq!(windows.grab.unwrap().zone, Some(Zone::Top));
        windows.finish(AppId::Control, false);
        let window = *windows.get(AppId::Control).unwrap();
        assert!(window.maximized);
        assert_eq!(window.frame(AREA), [0.0, 0.0, AREA[0], AREA[1]]);
    }

    #[test]
    fn dragging_a_maximized_title_restores_it_under_the_pointer() {
        let mut windows = opened(&[AppId::Control]);
        windows.toggle_maximized(AppId::Control);
        windows.start(AppId::Control, None, AREA);
        // Pressed at the middle of the title, then dragged down 100 points.
        windows.move_to(AppId::Control, [0.0, 100.0], [640.0, 120.0], AREA);
        let window = *windows.get(AppId::Control).unwrap();
        assert!(!window.maximized);
        let [x, y, w, h] = window.frame(AREA);
        assert_eq!([w, h], [1060.0, 700.0], "back to its own size");
        assert_eq!(x + w / 2.0, 640.0, "still held at the middle of its title");
        assert_eq!(y, 100.0);
    }

    #[test]
    fn edges_resize_down_to_the_minimum_and_the_top_stops_at_the_screen() {
        let mut windows = opened(&[AppId::Control]);
        let [min_w, min_h] = AppId::Control.min_size();
        windows.start(AppId::Control, Some(Edge::SE), AREA);
        windows.resize_to(AppId::Control, [-2000.0, -2000.0]);
        assert_eq!(
            windows.get(AppId::Control).unwrap().frame(AREA),
            [8.0, 8.0, min_w, min_h]
        );
        windows.finish(AppId::Control, false);

        windows.start(AppId::Control, Some(Edge::NW), AREA);
        windows.resize_to(AppId::Control, [-50.0, -50.0]);
        assert_eq!(
            windows.get(AppId::Control).unwrap().frame(AREA),
            [-42.0, 0.0, min_w + 50.0, min_h + 8.0]
        );
    }

    #[test]
    fn the_taskbar_minimizes_the_front_window_and_restores_or_raises_others() {
        let mut windows = opened(&[AppId::Control, AppId::Probe]);
        windows.press_task(AppId::Control);
        assert_eq!(windows.focused(), Some(AppId::Control), "raised");
        windows.press_task(AppId::Control);
        assert!(windows.get(AppId::Control).unwrap().minimized);
        assert_eq!(
            windows.focused(),
            Some(AppId::Probe),
            "the next window takes the focus"
        );
        windows.press_task(AppId::Control);
        assert!(!windows.get(AppId::Control).unwrap().minimized);
        assert_eq!(windows.focused(), Some(AppId::Control));
        windows.close(AppId::Control);
        assert_eq!(windows.focused(), Some(AppId::Probe));
        windows.minimize(AppId::Probe);
        assert_eq!(windows.focused(), None);
        assert!(windows.stacked().is_empty());
    }
}
