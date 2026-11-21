//! The classic RAST desktop, rebuilt with Rune. Backend apps are previews.

mod panels;
mod window;

use std::collections::HashMap;

use rune::{
    Color, Cx, El, Image, ImageCache, Key, KeyEvent, SurfaceFit, TextEdit, TextStyle, Tx, Waker,
    col, el, row, text,
};

use crate::{Screen, State, connection::Phase};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppId {
    Control,
}

impl AppId {
    pub(super) const fn key(self) -> &'static str {
        "controlpanel"
    }
    pub(super) const fn label(self) -> &'static str {
        "Connection settings"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Start,
    Account,
    Status,
    Clock,
    Appearance,
    Shortcuts,
    About,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) enum Wallpaper {
    #[default]
    Alpine,
    Forest,
    Coast,
    Desert,
    Mountain,
}

impl Wallpaper {
    pub(super) const ALL: [Self; 5] = [
        Self::Alpine,
        Self::Forest,
        Self::Coast,
        Self::Desert,
        Self::Mountain,
    ];
    fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        if let Some(key) = web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|storage| storage.get_item("rast.wallpaper").ok().flatten())
        {
            return Self::ALL
                .into_iter()
                .find(|item| item.key() == key)
                .unwrap_or_default();
        }
        Self::default()
    }
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Alpine => "alpine-dawn",
            Self::Forest => "forest-mist",
            Self::Coast => "coastal-calm",
            Self::Desert => "desert-dusk",
            Self::Mountain => "mountain-valley",
        }
    }
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Alpine => "Alpine Dawn",
            Self::Forest => "Forest Mist",
            Self::Coast => "Coastal Calm",
            Self::Desert => "Desert Dusk",
            Self::Mountain => "Mountain Valley",
        }
    }
}

pub(crate) struct Desktop {
    pub(crate) panel: Option<Panel>,
    pub(crate) search: TextEdit,
    pub(crate) active: Option<AppId>,
    opened: Vec<(AppId, bool)>,
    pub(crate) maximized: bool,
    dark: Option<bool>,
    pub(crate) wallpaper: Wallpaper,
    pub(crate) clock: String,
    pub(crate) date: String,
    next_clock: f64,
    art: Option<Art>,
}

impl Default for Desktop {
    fn default() -> Self {
        Self {
            panel: None,
            search: TextEdit::new(""),
            active: None,
            opened: Vec::new(),
            maximized: false,
            dark: None,
            wallpaper: Wallpaper::load(),
            clock: "09:41".into(),
            date: "21 Nov".into(),
            next_clock: 0.0,
            art: None,
        }
    }
}

impl Desktop {
    pub(crate) fn is_dark(&self) -> bool {
        self.dark.unwrap_or(true)
    }

    pub(crate) fn load_art(&mut self, wake: Waker) {
        self.art = Some(Art::load(wake));
    }

    pub(crate) fn close_session(&mut self) {
        self.panel = None;
        self.search = TextEdit::new("");
        self.active = None;
        self.opened.clear();
        self.maximized = false;
    }

    pub(crate) fn tick(&mut self, tx: &mut Tx) -> bool {
        if rune::is_headless() || tx.clock() + 0.01 < self.next_clock {
            return false;
        }
        use rune::time::{LocalTime, UNIX_EPOCH, system_now};
        let ms = system_now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let local = LocalTime::at(ms);
        let month = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let time = format!("{:02}:{:02}", local.hour, local.minute);
        let date = format!("{} {}", local.day, month[(local.month - 1) as usize]);
        self.next_clock = tx.clock() + f64::from(60 - local.second);
        tx.wake_at(self.next_clock);
        let changed = time != self.clock || date != self.date;
        self.clock = time;
        self.date = date;
        changed
    }
}

impl State {
    pub(crate) fn toggle_desktop_panel(&mut self, panel: Panel) {
        if self.desktop.panel == Some(panel) {
            self.dismiss_panel();
        } else {
            self.desktop.panel = Some(panel);
            if panel == Panel::Start {
                self.desktop.search = TextEdit::new("");
            }
        }
    }

    pub(crate) fn dismiss_panel(&mut self) {
        self.desktop.panel = None;
    }
    pub(crate) fn toggle_desktop_theme(&mut self) {
        self.desktop.dark = Some(!self.desktop.dark.unwrap_or(true));
    }
    pub(crate) fn set_wallpaper(&mut self, wallpaper: Wallpaper) {
        self.desktop.wallpaper = wallpaper;
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = storage.set_item("rast.wallpaper", wallpaper.key());
        }
    }

    pub(crate) fn open_app(&mut self, app: AppId) {
        self.dismiss_panel();
        if self.desktop.active == Some(app) {
            return;
        }
        self.desktop.maximized =
            if let Some((_, maximized)) = self.desktop.opened.iter().find(|(id, _)| *id == app) {
                *maximized
            } else {
                self.desktop.opened.push((app, false));
                if app == AppId::Control {
                    self.server_field = TextEdit::new(self.config.server.clone());
                    self.settings_error = None;
                }
                false
            };
        self.desktop.active = Some(app);
        self.screen = if app == AppId::Control {
            Screen::Settings
        } else {
            Screen::Desktop
        };
    }

    pub(crate) fn minimize_app(&mut self) {
        self.desktop.active = None;
        self.desktop.maximized = false;
        self.screen = Screen::Desktop;
    }

    pub(crate) fn close_app(&mut self) {
        if let Some(app) = self.desktop.active {
            self.desktop.opened.retain(|(id, _)| *id != app);
        }
        self.minimize_app();
    }

    pub(crate) fn toggle_app_maximized(&mut self) {
        let Some(active) = self.desktop.active else {
            return;
        };
        self.desktop.maximized = !self.desktop.maximized;
        if let Some((_, maximized)) = self.desktop.opened.iter_mut().find(|(id, _)| *id == active) {
            *maximized = self.desktop.maximized;
        }
    }
}

pub(crate) fn key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if !state.signed_in {
        return false;
    }
    if (event.mods.ctrl || event.mods.cmd)
        && !event.mods.alt
        && !event.mods.shift
        && (event.is_command(' ') || matches!(&event.key, Key::Text(value) if value == " "))
    {
        if state.desktop.panel != Some(Panel::Start) {
            state.toggle_desktop_panel(Panel::Start);
        }
        tx.focus("start-personalize", true);
        return true;
    }
    if event.key == Key::Escape && state.desktop.panel.is_some() {
        state.dismiss_panel();
        tx.focus("desktop-start", true);
        return true;
    }
    false
}

struct Art {
    wallpapers: [Image; 5],
    icons: HashMap<&'static str, Image>,
}

impl Art {
    fn load(wake: Waker) -> Self {
        let cache = ImageCache::new(wake);
        let wallpapers = [
            cache.load(&include_bytes!("../assets/alpine-dawn.jpg")[..]),
            cache.load(&include_bytes!("../assets/desktop/forest-mist.jpg")[..]),
            cache.load(&include_bytes!("../assets/desktop/coastal-calm.jpg")[..]),
            cache.load(&include_bytes!("../assets/desktop/desert-dusk.jpg")[..]),
            cache.load(&include_bytes!("../assets/desktop/mountain-valley.jpg")[..]),
        ];
        macro_rules! icons {
            ($($name:literal),+ $(,)?) => { [$(($name, cache.svg(&include_bytes!(concat!("../assets/desktop/", $name, ".svg"))[..], 34.0, 2.0))),+] };
        }
        let mut icons: HashMap<_, _> = icons!(
            "files",
            "applications",
            "monitor",
            "backup",
            "controlpanel",
            "logs",
            "logo",
            "account",
            "network",
            "clock",
            "theme",
            "moon",
            "info",
            "keyboard",
            "wallpaper"
        )
        .into();
        icons.insert("settings", icons["controlpanel"].clone());
        #[cfg(not(target_arch = "wasm32"))]
        if rune::is_headless() {
            for image in wallpapers.iter().chain(icons.values()) {
                let _ = image.wait();
            }
        }
        Self { wallpapers, icons }
    }
}

pub(super) fn wallpaper(state: &State, selected: Wallpaper) -> Option<&Image> {
    state
        .desktop
        .art
        .as_ref()
        .map(|art| &art.wallpapers[selected as usize])
}

pub(super) fn icon(state: &State, key: &str, size: f32) -> El {
    if let Some(mark) = state
        .desktop
        .art
        .as_ref()
        .and_then(|art| art.icons.get(key))
    {
        el().size(size, size)
            .pixels_fit(mark.pixels(), SurfaceFit::Fit)
    } else {
        el().size(size, size)
    }
}

fn rgb(value: &str) -> Color {
    rune::color::hex(value).expect("valid desktop palette")
}

pub(super) struct Palette {
    pub(super) surface: Color,
    pub(super) surface_2: Color,
    pub(super) line: Color,
    pub(super) ink: Color,
    pub(super) muted: Color,
    pub(super) accent: Color,
    pub(super) accent_ink: Color,
    pub(super) hover: Color,
    bar: Color,
}

impl Palette {
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                surface: rgb("#1e252e"),
                surface_2: rgb("#29323c"),
                line: rgb("#465360"),
                ink: rgb("#eff4fa"),
                muted: rgb("#afbfce"),
                accent: rgb("#3d91ea"),
                accent_ink: rgb("#0c1828"),
                hover: rgb("#344b63"),
                bar: rgb("#232a32"),
            }
        } else {
            Self {
                surface: rgb("#fbfcfe"),
                surface_2: rgb("#edf2f7"),
                line: rgb("#c6d2de"),
                ink: rgb("#233447"),
                muted: rgb("#50677c"),
                accent: rgb("#2374be"),
                accent_ink: Color::WHITE,
                hover: rgb("#dcecff"),
                bar: rgb("#edf2f8"),
            }
        }
    }
}

const BODY: TextStyle = TextStyle::new(13.0, 550.0);
const SMALL: TextStyle = TextStyle::new(11.0, 580.0);
const BAR: f32 = 54.0;

pub(crate) fn view(state: &State, cx: &mut Cx<State>) -> El {
    cx.web_focus();
    let dark = state.desktop.dark.unwrap_or(true);
    let p = Palette::new(dark);
    cx.focus_ring(2.0, -2.0, p.accent);
    let [width, height] = cx.layout_size("workspace-root").unwrap_or([1280.0, 800.0]);
    let compact = width < 680.0;
    let backdrop = el()
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .fill(rgb("#435668"));
    let backdrop = if let Some(photo) = wallpaper(state, state.desktop.wallpaper) {
        backdrop.pixels_fit(photo.pixels(), SurfaceFit::Fill)
    } else {
        backdrop
    };
    let mut root = col()
        .key("workspace-root")
        .w_full()
        .h_full()
        .child(backdrop)
        .child(
            el().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .gradient(rgb("#192739").alpha(0.12), rgb("#142330").alpha(0.25)),
        );
    if let Some(app) = state.desktop.active {
        root = root.child(window::view(state, &p, app, width, height, compact));
    }
    if let Some(panel) = panels::view(state, &p, width, height, compact) {
        root = root
            .child(
                el().key("desktop-dismiss-panel")
                    .label("Dismiss panel")
                    .focusable(false)
                    .pin(Some(0.0), Some(0.0), Some(0.0), Some(BAR))
                    .on_press(State::dismiss_panel),
            )
            .child(panel);
    }
    root.child(taskbar(state, &p, dark, width))
}

fn taskbar(state: &State, p: &Palette, dark: bool, width: f32) -> El {
    let compact = width < 680.0;
    let tiny = width < 360.0;
    let start = el()
        .key("desktop-start")
        .role("button")
        .label("Start")
        .expanded(state.desktop.panel == Some(Panel::Start))
        .size(38.0, 38.0)
        .radius(9.0)
        .center()
        .fill(p.accent)
        .hover(p.accent.mix(p.surface, 0.2))
        .child(icon(state, "logo", 22.0))
        .on_press(|s: &mut State| s.toggle_desktop_panel(Panel::Start));
    let who = state
        .account
        .as_ref()
        .map_or("Local user", |a| a.username.as_str());
    let initial = who
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".into());
    let account = row()
        .key("desktop-account")
        .role("button")
        .label("Local account")
        .h(40.0)
        .px(4.0)
        .center()
        .gap(8.0)
        .radius(7.0)
        .hover(p.hover)
        .child(
            el().size(29.0, 29.0)
                .radius(15.0)
                .fill(p.accent)
                .center()
                .child(text(initial).font(BODY).color(p.accent_ink)),
        );
    let account = if compact {
        account
    } else {
        account.child(
            col()
                .w(76.0)
                .gap(2.0)
                .child(text(short_name(who)).font(BODY).color(p.ink))
                .child(text("LOCAL").font(SMALL).color(p.muted)),
        )
    };
    let clock = col()
        .key("desktop-clock")
        .role("button")
        .label("Local time")
        .h(38.0)
        .px(5.0)
        .center()
        .radius(6.0)
        .hover(p.hover)
        .child(text(state.desktop.clock.clone()).font(BODY).color(p.ink));
    let clock = if compact {
        clock
    } else {
        clock.child(text(state.desktop.date.clone()).font(SMALL).color(p.muted))
    };
    let mut tray = row().gap(if compact { 2.0 } else { 6.0 }).center_y().child(
        tray_icon(state, p, "desktop-status", "Connection status", "network")
            .on_press(|s: &mut State| s.toggle_desktop_panel(Panel::Status)),
    );
    if !tiny {
        tray = tray.child(
            tray_icon(
                state,
                p,
                "desktop-theme",
                if dark {
                    "Switch to light theme"
                } else {
                    "Switch to dark theme"
                },
                if dark { "theme" } else { "moon" },
            )
            .on_press(State::toggle_desktop_theme),
        );
    }
    let tray = tray
        .child(clock.on_press(|s: &mut State| s.toggle_desktop_panel(Panel::Clock)))
        .child(account.on_press(|s: &mut State| s.toggle_desktop_panel(Panel::Account)));
    row()
        .key("desktop-taskbar")
        .pin(Some(0.0), None, Some(0.0), Some(0.0))
        .h(BAR)
        .px(if compact { 6.0 } else { 12.0 })
        .center_y()
        .gap(if compact { 4.0 } else { 9.0 })
        .fill(p.bar)
        .border_sides([1.0, 0.0, 0.0, 0.0], p.line)
        .child(start)
        .child(el().grow(1.0).min_w(0.0))
        .child(tray)
}

fn short_name(value: &str) -> String {
    let mut chars = value.chars();
    let mut name: String = chars.by_ref().take(10).collect();
    if chars.next().is_some() {
        name.push('…');
    }
    name
}

fn tray_icon(
    state: &State,
    p: &Palette,
    id: &'static str,
    label: &'static str,
    mark: &'static str,
) -> El {
    let mut item = el()
        .key(id)
        .role("button")
        .label(label)
        .size(30.0, 38.0)
        .radius(6.0)
        .hover(p.hover)
        .center()
        .child(icon(state, mark, 17.0));
    if id == "desktop-status" {
        let color = if cfg!(target_arch = "wasm32") {
            p.muted
        } else {
            match state.status.phase {
                Phase::Online => rgb("#66daa3"),
                Phase::Connecting | Phase::Setup => rgb("#f1c773"),
                Phase::Offline => rgb("#f18e8d"),
            }
        };
        item = item.child(el().size(6.0, 6.0).radius(4.0).fill(color).pin(
            None,
            Some(7.0),
            Some(3.0),
            None,
        ));
    }
    item
}

pub(super) fn sign_out_button(p: &Palette) -> El {
    crate::ui::bind_sign_out(
        row()
            .key("sign-out")
            .role("button")
            .label("Sign out")
            .w_full()
            .h(35.0)
            .px(8.0)
            .center_y()
            .radius(6.0)
            .hover(p.hover)
            .child(text("Sign out").font(BODY).color(p.ink)),
    )
}
