//! The classic RAST desktop, rebuilt with Rune. Backend apps are previews.

pub(crate) mod control;
mod panels;
mod system;
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
        "Control Panel"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Start,
    Account,
    Status,
    Device,
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
    pub(crate) clock: system::Clock,
    pub(crate) device: system::Device,
    /// Browser connectivity; natively the taskbar shows server reachability.
    pub(crate) network: system::Network,
    next_clock: f64,
    art: Option<Art>,
    #[cfg(target_arch = "wasm32")]
    watch: Option<system::Watch>,
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
            clock: system::Clock::sample(),
            #[cfg(target_arch = "wasm32")]
            device: system::device(),
            #[cfg(not(target_arch = "wasm32"))]
            device: system::sample_device(),
            #[cfg(target_arch = "wasm32")]
            network: system::network(),
            #[cfg(not(target_arch = "wasm32"))]
            network: system::Network {
                online: true,
                ..system::Network::default()
            },
            next_clock: 0.0,
            art: None,
            #[cfg(target_arch = "wasm32")]
            watch: None,
        }
    }
}

impl Desktop {
    pub(crate) fn is_dark(&self) -> bool {
        self.dark.unwrap_or(true)
    }

    /// Load the artwork, read this device and watch the browser's connection.
    pub(crate) fn load_art(&mut self, wake: Waker) {
        #[cfg(target_arch = "wasm32")]
        {
            self.watch = system::Watch::new(wake.clone());
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.device = system::device();
        }
        self.art = Some(Art::load(wake));
    }

    /// Re-read the browser's connection after an online, offline or change event.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn refresh_network(&mut self) -> bool {
        let network = system::network();
        let changed = network != self.network;
        self.network = network;
        changed
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
        let clock = system::Clock::at(
            local.year,
            local.month,
            local.day,
            local.hour,
            local.minute,
            local.offset,
        );
        self.next_clock = tx.clock() + f64::from(60 - local.second);
        tx.wake_at(self.next_clock);
        let changed = clock != self.clock;
        self.clock = clock;
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
                    self.reset_control();
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
    /// Darker versions of the pale line icons for the light theme.
    icons_light: HashMap<&'static str, Image>,
}

/// The line icons' pale strokes and the darker inks they take on light surfaces.
const LIGHT_INKS: [(&str, &str); 3] = [
    ("#a8b9c9", "#41566b"),
    ("#8DA3B8", "#4a6178"),
    ("#80c6ab", "#1d8a60"),
];

/// An icon's light-theme version, or `None` when it has no pale strokes.
fn for_light(svg: &[u8]) -> Option<Vec<u8>> {
    let source = std::str::from_utf8(svg).ok()?;
    let mut light = source.to_owned();
    for (pale, ink) in LIGHT_INKS {
        light = light.replace(pale, ink);
    }
    (light != source).then(|| light.into_bytes())
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
            ($($name:literal),+ $(,)?) => { [$(($name, &include_bytes!(concat!("../assets/desktop/", $name, ".svg"))[..])),+] };
        }
        let sources: [(&'static str, &'static [u8]); 18] = icons!(
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
            "wallpaper",
            "device",
            "users",
            "profile"
        );
        let mut icons: HashMap<_, _> = sources
            .iter()
            .map(|&(name, svg)| (name, cache.svg(svg, 34.0, 2.0)))
            .collect();
        icons.insert("settings", icons["controlpanel"].clone());
        let icons_light: HashMap<_, _> = sources
            .iter()
            .filter_map(|&(name, svg)| Some((name, cache.svg(for_light(svg)?, 34.0, 2.0))))
            .collect();
        #[cfg(not(target_arch = "wasm32"))]
        if rune::is_headless() {
            for image in wallpapers
                .iter()
                .chain(icons.values())
                .chain(icons_light.values())
            {
                let _ = image.wait();
            }
        }
        Self {
            wallpapers,
            icons,
            icons_light,
        }
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
    let light = !state.desktop.is_dark();
    if let Some(mark) = state.desktop.art.as_ref().and_then(|art| {
        light
            .then(|| art.icons_light.get(key))
            .flatten()
            .or_else(|| art.icons.get(key))
    }) {
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

const BODY: TextStyle = TextStyle::new(13.0, 450.0);
/// The taskbar height. Popovers and windows stay above it.
pub(super) const BAR: f32 = 44.0;
const CHIP: f32 = 32.0;
const LABEL: TextStyle = TextStyle::new(12.0, 450.0);
const NAME: TextStyle = TextStyle::new(12.0, 500.0);
const TIME: TextStyle = TextStyle::new(12.0, 500.0).features("'tnum' 1");
const CAPTION: TextStyle = TextStyle::new(10.5, 400.0);

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

/// Which tray details fit the window: labels on wide screens, icons on narrow ones.
struct Fit {
    network_label: bool,
    theme: bool,
    date: bool,
    name: bool,
    role: bool,
}

impl Fit {
    fn new(width: f32) -> Self {
        Self {
            network_label: width >= 820.0,
            theme: width >= 400.0,
            date: width >= 340.0,
            name: width >= 700.0,
            role: width >= 1024.0,
        }
    }
}

/// The internet (browser) or server (native) connection as the tray shows it.
pub(super) struct Connectivity {
    pub(super) label: &'static str,
    pub(super) color: Color,
}

pub(super) fn connectivity(state: &State) -> Connectivity {
    let (label, color) = if cfg!(target_arch = "wasm32") {
        if state.desktop.network.online {
            ("Online", "#66daa3")
        } else {
            ("Offline", "#f18e8d")
        }
    } else {
        match state.status.phase {
            Phase::Online => ("Online", "#66daa3"),
            Phase::Connecting => ("Connecting", "#f1c773"),
            Phase::Setup => ("Not set up", "#f1c773"),
            Phase::Offline => ("Offline", "#f18e8d"),
        }
    };
    Connectivity {
        label,
        color: rgb(color),
    }
}

pub(super) fn initial(name: &str) -> String {
    name.chars()
        .next()
        .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
}

fn taskbar(state: &State, p: &Palette, dark: bool, width: f32) -> El {
    let fit = Fit::new(width);
    let open = state.desktop.panel;
    let start = el()
        .key("desktop-start")
        .role("button")
        .label("Start")
        .expanded(open == Some(Panel::Start))
        .size(CHIP, CHIP)
        .radius(8.0)
        .center()
        .fill(p.accent)
        .hover(p.accent.mix(p.surface, 0.2))
        .child(icon(state, "logo", 19.0))
        .on_press(|s: &mut State| s.toggle_desktop_panel(Panel::Start));

    let mut tray = row().gap(2.0).center_y();

    let net = connectivity(state);
    let kind = if cfg!(target_arch = "wasm32") {
        "Internet"
    } else {
        "Server"
    };
    let mut network = chip(
        p,
        "desktop-status",
        format!("{kind}: {}", net.label),
        open == Some(Panel::Status),
        fit.network_label,
    )
    .child(
        el().size(16.0, 16.0)
            .a11y_hidden()
            .child(icon(state, "network", 16.0))
            .child(
                el().size(7.0, 7.0)
                    .radius(4.0)
                    .fill(net.color)
                    .border(1.0, p.bar)
                    .pin(None, Some(-2.0), Some(-3.0), None),
            ),
    );
    if fit.network_label {
        network = network.child(text(net.label).font(LABEL).color(p.ink).a11y_hidden());
    }
    tray = tray.child(network.on_press(|s: &mut State| s.toggle_desktop_panel(Panel::Status)));

    if fit.theme {
        tray = tray.child(
            chip(
                p,
                "desktop-theme",
                if dark {
                    "Switch to light theme"
                } else {
                    "Switch to dark theme"
                },
                false,
                false,
            )
            .child(icon(state, if dark { "theme" } else { "moon" }, 16.0).a11y_hidden())
            .on_press(State::toggle_desktop_theme),
        );
    }

    tray = tray.child(separator(p));

    let clock = &state.desktop.clock;
    let mut time = col()
        .key("desktop-clock")
        .role("button")
        .label(format!("{}, {}", clock.time, clock.long_date))
        .expanded(open == Some(Panel::Clock))
        .h(CHIP)
        .px(8.0)
        .center()
        .radius(7.0)
        .hover(p.hover)
        .child(
            text(clock.time.clone())
                .font(TIME)
                .color(p.ink)
                .a11y_hidden(),
        );
    if open == Some(Panel::Clock) {
        time = time.fill(p.hover);
    }
    if fit.date {
        time = time.child(
            text(clock.short_date.clone())
                .font(CAPTION)
                .color(p.muted)
                .a11y_hidden(),
        );
    }
    tray = tray.child(time.on_press(|s: &mut State| s.toggle_desktop_panel(Panel::Clock)));

    let who = state.user_name();
    let mut account = chip(
        p,
        "desktop-account",
        format!("Account: {who}"),
        open == Some(Panel::Account),
        fit.name,
    )
    .child(
        el().size(24.0, 24.0)
            .radius(12.0)
            .fill(p.accent)
            .center()
            .a11y_hidden()
            .child(
                text(initial(who))
                    .font(TextStyle::new(11.5, 600.0))
                    .color(p.accent_ink),
            ),
    );
    if fit.name {
        let mut names = col()
            .max_w(120.0)
            .a11y_hidden()
            .child(text(who.to_owned()).font(NAME).color(p.ink).truncate());
        if fit.role {
            names = names.child(text(state.role_label()).font(CAPTION).color(p.muted));
        }
        account = account.child(names);
    }
    tray = tray.child(account.on_press(|s: &mut State| s.toggle_desktop_panel(Panel::Account)));

    row()
        .key("desktop-taskbar")
        .pin(Some(0.0), None, Some(0.0), Some(0.0))
        .h(BAR)
        .px(if width < 480.0 { 4.0 } else { 6.0 })
        .center_y()
        .gap(4.0)
        .fill(p.bar.alpha(0.92))
        .border_sides([1.0, 0.0, 0.0, 0.0], p.line.alpha(0.7))
        .child(start)
        .child(el().grow(1.0).min_w(0.0))
        .child(tray)
}

/// A tray button: icon only, or icon and label when `labelled`.
fn chip(
    p: &Palette,
    key: &'static str,
    label: impl Into<String>,
    open: bool,
    labelled: bool,
) -> El {
    let item = row()
        .key(key)
        .role("button")
        .label(label)
        .h(CHIP)
        .gap(6.0)
        .radius(7.0)
        .hover(p.hover)
        .expanded(open);
    let item = if labelled {
        item.px(8.0).center_y()
    } else {
        item.w(CHIP).center()
    };
    if open { item.fill(p.hover) } else { item }
}

fn separator(p: &Palette) -> El {
    el().w(1.0)
        .h(18.0)
        .mx(4.0)
        .fill(p.line.alpha(0.8))
        .a11y_hidden()
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
