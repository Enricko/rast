//! The classic RAST desktop, rebuilt with Rune. Backend apps are previews.

pub(crate) mod appearance;
pub(crate) mod control;
mod glyphs;
mod menu;
mod panels;
mod personalize;
mod system;
mod window;
pub(crate) mod windows;

use std::cell::Cell;
use std::collections::HashMap;

use rune::{
    Color, Cx, El, Image, ImageCache, Key, KeyEvent, SurfaceFit, TextEdit, TextStyle, Tx, Waker,
    col, el, row, text,
};

use crate::{State, connection::Phase};
use appearance::{Accent, Appearance, Fit, Kind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppId {
    Control,
    /// A second window for the window manager's tests.
    #[cfg(test)]
    Probe,
}

impl AppId {
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Control => "controlpanel",
            #[cfg(test)]
            Self::Probe => "probe",
        }
    }
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Control => "Control Panel",
            #[cfg(test)]
            Self::Probe => "Probe",
        }
    }
    /// The id groups and rules use for this app (see the server's `access`).
    pub(crate) const fn access_id(self) -> &'static str {
        self.key()
    }
    /// The size its window opens at where the screen has room (the old RAST's sizes).
    pub(crate) const fn open_size(self) -> [f32; 2] {
        match self {
            Self::Control => [1060.0, 700.0],
            #[cfg(test)]
            Self::Probe => [940.0, 620.0],
        }
    }
    /// The smallest its window resizes to.
    pub(crate) const fn min_size(self) -> [f32; 2] {
        match self {
            Self::Control => [420.0, 380.0],
            #[cfg(test)]
            Self::Probe => [360.0, 260.0],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Start,
    Account,
    Status,
    Device,
    Clock,
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
    /// The line under its thumbnail in Personalize.
    pub(super) const fn caption(self) -> &'static str {
        match self {
            Self::Alpine => "Calm lake below snow-capped peaks at sunrise",
            Self::Forest => "Pine forest with soft morning light rays",
            Self::Coast => "Turquoise water, smooth rocks and a pale sky",
            Self::Desert => "Warm dunes under a violet evening sky",
            Self::Mountain => "A river winding through pines below snowy peaks",
        }
    }
}

pub(crate) struct Desktop {
    pub(crate) panel: Option<Panel>,
    pub(crate) search: TextEdit,
    pub(crate) windows: windows::Windows,
    /// The account's appearance and Personalize's draft.
    pub(crate) look: appearance::Personalize,
    /// The operating system's colour mode as last drawn, for *System*.
    system_dark: Cell<bool>,
    /// The desktop's right-click menu, open at this point.
    pub(crate) menu: Option<[f32; 2]>,
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
            windows: windows::Windows::default(),
            look: appearance::Personalize::default(),
            system_dark: Cell::new(false),
            menu: None,
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
    /// Dark colours, as the shown appearance (the draft while Personalize previews one)
    /// resolves them.
    pub(crate) fn is_dark(&self) -> bool {
        self.look.shown().dark(self.system_dark.get())
    }

    /// The app whose window is in front and takes the keyboard.
    pub(crate) fn active(&self) -> Option<AppId> {
        self.windows.focused()
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
        self.menu = None;
        self.search = TextEdit::new("");
        self.windows.clear();
        let look = &mut self.look;
        look.draft = None;
        look.dialog = false;
        look.failed = None;
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
            // The menus list Control Panel only when it may be opened, so ask
            // again in case a Super Admin changed this account's groups.
            if matches!(panel, Panel::Start | Panel::Account) {
                self.refresh_access();
            }
        }
    }

    pub(crate) fn dismiss_panel(&mut self) {
        self.desktop.panel = None;
    }

    /// Right-click on the desktop: its menu, at the pointer.
    pub(crate) fn open_desktop_menu(&mut self, at: [f32; 2]) {
        self.dismiss_panel();
        self.desktop.menu = Some(at);
    }

    pub(crate) fn close_desktop_menu(&mut self) {
        self.desktop.menu = None;
    }
}

pub(crate) fn key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    // Personalize and the desktop's menu take their own keys (Escape closes them).
    if !state.signed_in || state.desktop.look.dialog || state.desktop.menu.is_some() {
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
        let sources: [(&'static str, &'static [u8]); 19] = icons!(
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
            "profile",
            "groups"
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

/// `t` of the way from `a` to `b`, mixed as CSS `color-mix(in srgb, …)` does.
fn srgb_mix(a: Color, b: Color, t: f32) -> Color {
    use rune::color::{srgb_decode, srgb_encode};
    let m = |x: f32, y: f32| srgb_decode(srgb_encode(x) + (srgb_encode(y) - srgb_encode(x)) * t);
    Color {
        r: m(a.r, b.r),
        g: m(a.g, b.g),
        b: m(a.b, b.b),
        a: a.a + (b.a - a.a) * t,
    }
}

/// `t` of the way from `a` to `b` in Oklab, as CSS `color-mix(in oklab, …)` does.
fn oklab_mix(a: Color, b: Color, t: f32) -> Color {
    let (x, y) = (a.to_oklab(), b.to_oklab());
    Color::from_oklab([0, 1, 2, 3].map(|i| x[i] + (y[i] - x[i]) * t))
}

/// Classic RAST's colours for one mode and accent.
pub(super) struct Palette {
    pub(super) dark: bool,
    /// Windows, menus and dialogs.
    pub(super) surface: Color,
    /// Title bars, cards and quiet fills.
    pub(super) surface_2: Color,
    /// Pressed controls and empty picture frames.
    pub(super) surface_3: Color,
    /// Fields, buttons and the window frame.
    pub(super) line: Color,
    /// Cards and dividers.
    pub(super) subtle: Color,
    pub(super) ink: Color,
    pub(super) muted: Color,
    /// Hints and captions.
    pub(super) faint: Color,
    pub(super) accent: Color,
    /// Text and icons on the accent.
    pub(super) accent_ink: Color,
    pub(super) accent_hover: Color,
    /// Links and selected labels.
    pub(super) accent_text: Color,
    /// The accent's soft tint: selected and hovered items.
    pub(super) hover: Color,
    /// The border of a selected item.
    pub(super) accent_line: Color,
    /// A neutral hover for plain buttons and menu rows.
    pub(super) neutral: Color,
    pub(super) danger: Color,
    /// Status text: saved, unsaved, failed.
    pub(super) ok_text: Color,
    pub(super) warn_text: Color,
    pub(super) danger_text: Color,
    bar: Color,
}

impl Palette {
    pub(super) fn new(dark: bool, accent: Accent) -> Self {
        let (accent, accent_ink) = accent.colors(dark);
        let [
            surface,
            surface_2,
            surface_3,
            neutral,
            subtle,
            line,
            ink,
            muted,
            faint,
            danger,
            ok_text,
            warn_text,
            danger_text,
        ] = if dark {
            [
                "#20262e", "#29323d", "#333c47", "#2d3641", "#3b4551", "#4a5563", "#e8edf3",
                "#b3bdc9", "#9aa8b6", "#ef7d7d", "#6bd297", "#f0c67a", "#f59a9a",
            ]
        } else {
            [
                "#ffffff", "#f3f5f7", "#e9edf1", "#eef1f4", "#dce1e7", "#c7cfd8", "#26323f",
                "#4e5a67", "#5f6b78", "#c93b3b", "#1d7a42", "#7f5008", "#a92f2f",
            ]
        }
        .map(rgb);
        Self {
            dark,
            surface,
            surface_2,
            surface_3,
            line,
            subtle,
            ink,
            muted,
            faint,
            accent,
            accent_ink,
            accent_hover: if dark {
                oklab_mix(accent, Color::WHITE, 0.12)
            } else {
                oklab_mix(accent, Color::BLACK, 0.14)
            },
            accent_text: if dark {
                accent
            } else {
                oklab_mix(accent, ink, 0.18)
            },
            hover: srgb_mix(surface, accent, if dark { 0.18 } else { 0.12 }),
            accent_line: srgb_mix(subtle, accent, 0.42),
            neutral,
            danger,
            ok_text,
            warn_text,
            danger_text,
            bar: if dark {
                rgb("#232a32").alpha(0.92)
            } else {
                Color::WHITE.alpha(0.9)
            },
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
    state.desktop.system_dark.set(cx.system_dark());
    let look = state.desktop.look.shown();
    let dark = state.desktop.is_dark();
    let p = Palette::new(dark, look.accent);
    cx.focus_ring(2.0, -2.0, p.accent);
    let [width, height] = cx.layout_size("workspace-root").unwrap_or([1280.0, 800.0]);
    let compact = width < 680.0;
    let mut root = col()
        .key("workspace-root")
        .w_full()
        .h_full()
        .child(backdrop(state, look, [width, height]));
    for layer in window::view(state, &p, [width, (height - BAR).max(0.0)]) {
        root = root.child(layer);
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
    root = root.child(taskbar(state, &p, dark, width));
    for layer in control::popups(state, &p) {
        root = root.child(layer);
    }
    if let Some(at) = state.desktop.menu {
        for layer in menu::view(state, &p, at, [width, height]) {
            root = root.child(layer);
        }
    }
    if state.desktop.look.dialog {
        for layer in personalize::dialog(state, &p, [width, height]) {
            root = root.child(layer);
        }
    }
    root
}

/// The wallpaper: a picture fitted and placed as chosen, or one colour, under the dark
/// overlay. A right-click on it opens the desktop's menu.
fn backdrop(state: &State, look: &Appearance, [width, height]: [f32; 2]) -> El {
    let mut backdrop = el()
        .key("desktop-wallpaper")
        .focusable(false)
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .fill(appearance::hex(&look.color));
    if look.kind == Kind::Picture
        && let Some(photo) = wallpaper(state, look.picture)
    {
        let (column, line) = look.position.align();
        backdrop = match look.fit {
            Fit::Cover => backdrop
                .pixels_fit(photo.pixels(), SurfaceFit::Fill)
                .surface_anchor(column, line),
            Fit::Contain => backdrop
                .pixels_fit(photo.pixels(), SurfaceFit::Fit)
                .surface_anchor(column, line),
            // The picture at its own size (a point per pixel), in the middle.
            Fit::Center => {
                let [w, h] = photo
                    .size()
                    .map_or([1920.0, 1080.0], |[w, h]| [w as f32, h as f32]);
                backdrop.child(
                    el().size(w, h)
                        .absolute(((width - w) / 2.0).round(), ((height - h) / 2.0).round())
                        .pixels_fit(photo.pixels(), SurfaceFit::Fill),
                )
            }
        };
    }
    backdrop
        .child(
            el().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .fill(rgb("#0a0f16").alpha(f32::from(look.overlay) / 100.0)),
        )
        .on_press(State::close_desktop_menu)
        .on_secondary_press(|s: &mut State, at, _: &mut Tx| s.open_desktop_menu([at.x, at.y]))
}

/// Which tray details fit the window: labels on wide screens, icons on narrow ones.
struct TrayFit {
    network_label: bool,
    theme: bool,
    date: bool,
    name: bool,
    role: bool,
}

impl TrayFit {
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
    let fit = TrayFit::new(width);
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
        .hover(p.accent_hover)
        .child(
            rune::icon(&glyphs::LOGO, 19.0)
                .color(p.accent_ink)
                .a11y_hidden(),
        )
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
                    .border(1.0, p.bar.alpha(1.0))
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
        .fill(p.bar)
        .border_sides(
            [1.0, 0.0, 0.0, 0.0],
            if p.dark {
                Color::WHITE.alpha(0.07)
            } else {
                p.subtle
            },
        )
        .child(start)
        .child(tasks(state, p, width))
        .child(el().grow(1.0).min_w(0.0))
        .child(tray)
}

const TASK: TextStyle = TextStyle::new(12.0, 450.0);
const TASK_ACTIVE: TextStyle = TextStyle::new(12.0, 600.0);

/// A button for each open window, in the order they opened: the one in front is tinted with
/// the accent and marked with an accent pill, the others with a dot (dimmer when minimized).
fn tasks(state: &State, p: &Palette, width: f32) -> El {
    let wm = &state.desktop.windows;
    let icons_only = width <= 820.0;
    let mut strip = row().key("desktop-tasks").shrink().gap(4.0).center_y();
    for window in wm.all() {
        let app = window.app;
        let active = wm.focused() == Some(app);
        let marker = if active {
            el().size(14.0, 4.0).radius(2.0).fill(p.accent)
        } else {
            el().size(4.0, 4.0)
                .radius(2.0)
                .fill(p.muted.alpha(if window.minimized { 0.55 } else { 0.9 }))
        };
        let title = window::title(state, app);
        let mut button = row()
            .key(format!("task-{}", app.key()))
            .role("button")
            .label(if window.minimized {
                format!("{} (minimized)", app.label())
            } else {
                app.label().to_owned()
            })
            .help(if window.minimized {
                format!("{title} (minimized)")
            } else {
                title
            })
            .h(CHIP)
            .gap(7.0)
            .radius(7.0)
            .border(1.0, Color::TRANSPARENT)
            .hover(p.accent.alpha(if p.dark { 0.16 } else { 0.10 }))
            .child(icon(state, app.key(), 16.0).a11y_hidden())
            .child(
                row()
                    .pin(Some(0.0), None, Some(0.0), Some(2.0))
                    .h(4.0)
                    .center()
                    .a11y_hidden()
                    .child(marker),
            )
            .on_press(move |s: &mut State| s.press_task(app));
        button = if icons_only {
            button.w(36.0).center()
        } else {
            button.px(10.0).max_w(210.0).shrink().center_y().child(
                text(app.label())
                    .font(if active { TASK_ACTIVE } else { TASK })
                    .color(if window.minimized { p.muted } else { p.ink })
                    .min_w(0.0)
                    .truncate()
                    .a11y_hidden(),
            )
        };
        if active {
            let (tint, edge) = if p.dark { (0.25, 0.52) } else { (0.17, 0.44) };
            button = button
                .fill(p.accent.alpha(tint))
                .border(1.0, p.accent.alpha(edge));
        }
        strip = strip.child(button);
    }
    strip
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
