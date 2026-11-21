//! Appearance, as classic RAST's *Personalize* offered it: a colour mode, one of ten
//! accent colours and the wallpaper (a bundled picture or a solid colour, how it fits,
//! where it sits and how much it is darkened).
//!
//! The server keeps each account's choice (`/api/appearance`) so the desktop looks the
//! same in every browser. The browser keeps a copy too, so the desktop opens in the
//! right colours before the server answers. The editor (the Personalize dialog and
//! Control Panel → Appearance) previews a draft on the live desktop until it is applied
//! or cancelled.

use rune::{Align, Color, TextEdit};
use serde::{Deserialize, Serialize};

use super::Wallpaper;
use crate::State;
use crate::server::Failure;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Theme {
    #[default]
    Light,
    Dark,
    /// Follow the operating system, live.
    System,
}

impl Theme {
    pub(crate) const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::System => "System",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.key() == value)
    }
}

/// The ten accent presets. Light accents are dark enough for white text on them; dark
/// accents are light enough for their dark ink.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Accent {
    #[default]
    Ocean,
    Forest,
    Teal,
    Sage,
    Amber,
    Terracotta,
    Rose,
    Plum,
    Indigo,
    Graphite,
}

impl Accent {
    pub(crate) const ALL: [Self; 10] = [
        Self::Ocean,
        Self::Forest,
        Self::Teal,
        Self::Sage,
        Self::Amber,
        Self::Terracotta,
        Self::Rose,
        Self::Plum,
        Self::Indigo,
        Self::Graphite,
    ];
    /// Id, name, light colour, dark colour, ink on the dark colour.
    const fn spec(
        self,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ) {
        match self {
            Self::Ocean => ("ocean", "Ocean Blue", "#2563EB", "#7CB3F4", "#0f1a26"),
            Self::Forest => ("forest", "Forest Green", "#15803D", "#79BC8B", "#0f1a12"),
            Self::Teal => ("teal", "Teal", "#0F766E", "#6CCABD", "#0d1a18"),
            Self::Sage => ("sage", "Sage", "#4D7C0F", "#BECCA5", "#1a2010"),
            Self::Amber => ("amber", "Amber", "#B45309", "#E9B560", "#241704"),
            Self::Terracotta => ("terracotta", "Terracotta", "#B84D34", "#E99A80", "#251009"),
            Self::Rose => ("rose", "Rose", "#BE3455", "#EE93A9", "#260d15"),
            Self::Plum => ("plum", "Plum", "#7E3F98", "#CCA1DB", "#1d0f24"),
            Self::Indigo => ("indigo", "Indigo", "#4F46B8", "#ABA7EF", "#12102a"),
            Self::Graphite => ("graphite", "Graphite", "#475569", "#BBC7D4", "#151b22"),
        }
    }
    pub(crate) const fn key(self) -> &'static str {
        self.spec().0
    }
    pub(crate) const fn label(self) -> &'static str {
        self.spec().1
    }
    /// The colour as the editor names it, in the light or dark mode.
    pub(crate) const fn hex(self, dark: bool) -> &'static str {
        if dark { self.spec().3 } else { self.spec().2 }
    }
    /// The accent and the ink drawn on it.
    pub(crate) fn colors(self, dark: bool) -> (Color, Color) {
        let (_, _, light, on_dark, dark_ink) = self.spec();
        if dark {
            (hex(on_dark), hex(dark_ink))
        } else {
            (hex(light), Color::WHITE)
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.key() == value)
    }
}

/// A bundled picture, or one solid colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Kind {
    #[default]
    Picture,
    Color,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Fit {
    /// Fill the desktop, cropping the picture.
    #[default]
    Cover,
    /// Show all of the picture, with the colour around it.
    Contain,
    /// The picture at its own size.
    Center,
}

impl Fit {
    pub(crate) const ALL: [Self; 3] = [Self::Cover, Self::Contain, Self::Center];
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Contain => "contain",
            Self::Center => "center",
        }
    }
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Cover => "Cover",
            Self::Contain => "Contain",
            Self::Center => "Center",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.key() == value)
    }
}

/// Where the picture sits, as a 3 × 3 grid read row by row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Position {
    TopLeft,
    Top,
    TopRight,
    Left,
    #[default]
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

impl Position {
    pub(crate) const ALL: [Self; 9] = [
        Self::TopLeft,
        Self::Top,
        Self::TopRight,
        Self::Left,
        Self::Center,
        Self::Right,
        Self::BottomLeft,
        Self::Bottom,
        Self::BottomRight,
    ];
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::Top => "top",
            Self::TopRight => "top-right",
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
            Self::BottomLeft => "bottom-left",
            Self::Bottom => "bottom",
            Self::BottomRight => "bottom-right",
        }
    }
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::TopLeft => "Top left",
            Self::Top => "Top",
            Self::TopRight => "Top right",
            Self::Left => "Left",
            Self::Center => "Center",
            Self::Right => "Right",
            Self::BottomLeft => "Bottom left",
            Self::Bottom => "Bottom",
            Self::BottomRight => "Bottom right",
        }
    }
    /// Horizontal and vertical anchors.
    pub(crate) const fn align(self) -> (Align, Align) {
        let column = match self {
            Self::TopLeft | Self::Left | Self::BottomLeft => Align::Start,
            Self::Top | Self::Center | Self::Bottom => Align::Center,
            Self::TopRight | Self::Right | Self::BottomRight => Align::End,
        };
        let line = match self {
            Self::TopLeft | Self::Top | Self::TopRight => Align::Start,
            Self::Left | Self::Center | Self::Right => Align::Center,
            Self::BottomLeft | Self::Bottom | Self::BottomRight => Align::End,
        };
        (column, line)
    }
    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.key() == value)
    }
}

/// The solid colours on offer; the first is the default.
pub(crate) const SOLID_COLORS: [&str; 8] = [
    "#1B2430", "#27313D", "#3C4754", "#4B5560", "#5B6B5F", "#2E4756", "#443C5C", "#6B4F3F",
];
/// The darkening over the wallpaper goes up to this percentage, in steps of 5.
pub(crate) const MAX_OVERLAY: u8 = 60;

pub(crate) fn hex(value: &str) -> Color {
    rune::color::hex(value).unwrap_or(Color::BLACK)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Appearance {
    pub(crate) theme: Theme,
    pub(crate) accent: Accent,
    pub(crate) kind: Kind,
    pub(crate) picture: Wallpaper,
    /// `#RRGGBB`: the solid colour, and what shows around a contained picture.
    pub(crate) color: String,
    pub(crate) fit: Fit,
    pub(crate) position: Position,
    /// Percent, 0 to [`MAX_OVERLAY`].
    pub(crate) overlay: u8,
}

impl Default for Appearance {
    /// Classic RAST's first look: light, Ocean Blue, Alpine Dawn.
    fn default() -> Self {
        Self {
            theme: Theme::Light,
            accent: Accent::Ocean,
            kind: Kind::Picture,
            picture: Wallpaper::Alpine,
            color: SOLID_COLORS[0].to_owned(),
            fit: Fit::Cover,
            position: Position::Center,
            overlay: 20,
        }
    }
}

/// The JSON the server speaks.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wire {
    theme: String,
    accent: String,
    wallpaper: WireWallpaper,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireWallpaper {
    kind: String,
    builtin_id: String,
    color: String,
    fit: String,
    position: String,
    overlay: i64,
}

#[derive(Deserialize)]
pub(crate) struct Body {
    appearance: Wire,
}

impl Appearance {
    /// Dark under this mode, with the operating system's own preference given.
    pub(crate) fn dark(&self, system_dark: bool) -> bool {
        match self.theme {
            Theme::Light => false,
            Theme::Dark => true,
            Theme::System => system_dark,
        }
    }

    /// The same look with the default wallpaper, fit, position and overlay: classic
    /// RAST's *Reset to default* keeps the colour mode and accent.
    pub(crate) fn with_default_wallpaper(&self) -> Self {
        Self {
            theme: self.theme,
            accent: self.accent,
            ..Self::default()
        }
    }

    fn wire(&self) -> Wire {
        Wire {
            theme: self.theme.key().to_owned(),
            accent: self.accent.key().to_owned(),
            wallpaper: WireWallpaper {
                kind: match self.kind {
                    Kind::Picture => "builtin",
                    Kind::Color => "color",
                }
                .to_owned(),
                builtin_id: self.picture.key().to_owned(),
                color: self.color.clone(),
                fit: self.fit.key().to_owned(),
                position: self.position.key().to_owned(),
                overlay: i64::from(self.overlay),
            },
        }
    }

    pub(crate) fn to_json(&self) -> String {
        serde_json::to_string(&self.wire()).unwrap_or_default()
    }

    /// What the server or this browser's copy said, with anything unknown at its default.
    fn from_wire(wire: Wire) -> Self {
        let fallback = Self::default();
        let wallpaper = wire.wallpaper;
        let color = wallpaper.color.to_ascii_uppercase();
        let valid_color = color.len() == 7
            && color.starts_with('#')
            && color[1..].bytes().all(|b| b.is_ascii_hexdigit());
        Self {
            theme: Theme::parse(&wire.theme).unwrap_or(fallback.theme),
            accent: Accent::parse(&wire.accent).unwrap_or(fallback.accent),
            kind: if wallpaper.kind == "color" {
                Kind::Color
            } else {
                Kind::Picture
            },
            picture: Wallpaper::ALL
                .into_iter()
                .find(|w| w.key() == wallpaper.builtin_id)
                .unwrap_or(fallback.picture),
            color: if valid_color { color } else { fallback.color },
            fit: Fit::parse(&wallpaper.fit).unwrap_or(fallback.fit),
            position: Position::parse(&wallpaper.position).unwrap_or(fallback.position),
            overlay: wallpaper.overlay.clamp(0, i64::from(MAX_OVERLAY)) as u8,
        }
    }

    /// This browser's cached copy, read back.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn from_json(body: &str) -> Option<Self> {
        serde_json::from_str::<Wire>(body).ok().map(Self::from_wire)
    }

    pub(crate) fn from_body(body: Body) -> Self {
        Self::from_wire(body.appearance)
    }
}

/// What the editor's save bar says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status<'a> {
    /// Nothing to save: *All changes saved*.
    Clean,
    /// *Unsaved changes*, waiting for Apply or Save.
    Dirty,
    Saving,
    /// The server refused or could not be reached: *Could not save*, and why.
    Failed(&'a str),
}

/// The account's appearance and the editor's draft.
pub(crate) struct Personalize {
    /// The account's saved appearance.
    pub(crate) saved: Appearance,
    /// Unsaved changes, previewed on the desktop while an editor is open.
    pub(crate) draft: Option<Appearance>,
    /// The Personalize dialog is open (Control Panel → Appearance edits the same draft).
    pub(crate) dialog: bool,
    pub(crate) saving: bool,
    /// Why the last save failed, until the next change.
    pub(crate) failed: Option<String>,
    /// The solid colour typed as `#RRGGBB`.
    pub(crate) hex: TextEdit,
}

impl Default for Personalize {
    fn default() -> Self {
        let saved = Appearance::default();
        Self {
            hex: TextEdit::new(saved.color.clone()),
            saved,
            draft: None,
            dialog: false,
            saving: false,
            failed: None,
        }
    }
}

impl Personalize {
    /// What the desktop shows: the draft while there is one.
    pub(crate) fn shown(&self) -> &Appearance {
        self.draft.as_ref().unwrap_or(&self.saved)
    }

    /// Unsaved changes that differ from the saved look.
    pub(crate) fn changed(&self) -> bool {
        self.draft
            .as_ref()
            .is_some_and(|draft| *draft != self.saved)
    }

    pub(crate) fn status(&self) -> Status<'_> {
        if self.saving {
            Status::Saving
        } else if let Some(reason) = &self.failed {
            Status::Failed(reason)
        } else if self.changed() {
            Status::Dirty
        } else {
            Status::Clean
        }
    }

    /// Show the colour being edited in the `#RRGGBB` field.
    pub(crate) fn sync_hex(&mut self) {
        self.hex = TextEdit::new(self.shown().color.clone());
    }
}

/// `#RRGGBB` (any case, `#` optional), upper-cased; `None` for anything else.
pub(crate) fn parse_hex(value: &str) -> Option<String> {
    let digits = value.trim();
    let digits = digits.strip_prefix('#').unwrap_or(digits);
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("#{}", digits.to_ascii_uppercase()))
}

/// This browser's copy of an account's appearance, for the first paint after a reload.
#[cfg(target_arch = "wasm32")]
fn cache_key(user: i64) -> String {
    format!("rast.appearance.v1.{user}")
}

#[cfg(target_arch = "wasm32")]
fn storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

impl State {
    /// Show this browser's copy of the account's appearance, then ask the server.
    pub(crate) fn load_appearance(&mut self) {
        let Some(user) = self.session.as_ref().map(|session| session.id) else {
            return;
        };
        #[cfg(target_arch = "wasm32")]
        if let Some(cached) = storage()
            .and_then(|s| s.get_item(&cache_key(user)).ok().flatten())
            .and_then(|body| Appearance::from_json(&body))
        {
            self.desktop.look.saved = cached;
            self.desktop.look.sync_hex();
        }
        let _ = user;
        self.calls().appearance();
    }

    fn remember_appearance(&self) {
        #[cfg(target_arch = "wasm32")]
        if let (Some(user), Some(storage)) = (self.session.as_ref().map(|s| s.id), storage()) {
            let _ = storage.set_item(&cache_key(user), &self.desktop.look.saved.to_json());
        }
    }

    /// Change the draft with `edit`, starting one from the saved look.
    pub(crate) fn edit_appearance(&mut self, edit: impl FnOnce(&mut Appearance)) {
        if self.desktop.look.saving {
            return;
        }
        let look = &mut self.desktop.look;
        let draft = look.draft.get_or_insert_with(|| look.saved.clone());
        let color = draft.color.clone();
        edit(draft);
        if draft.color != color {
            look.sync_hex();
        }
        look.failed = None;
    }

    /// Use the colour typed in the `#RRGGBB` field, if it is one.
    pub(crate) fn apply_hex(&mut self) {
        let Some(color) = parse_hex(&self.desktop.look.hex.text) else {
            self.desktop.look.sync_hex();
            return;
        };
        self.edit_appearance(|look| {
            look.kind = Kind::Color;
            look.color = color;
        });
        self.desktop.look.sync_hex();
    }

    pub(crate) fn open_personalize(&mut self) {
        self.dismiss_panel();
        self.desktop.menu = None;
        let look = &mut self.desktop.look;
        look.dialog = true;
        look.sync_hex();
    }

    /// Cancel, Discard changes, or leaving Control Panel → Appearance: back to the saved look.
    pub(crate) fn discard_appearance(&mut self) {
        let look = &mut self.desktop.look;
        if look.saving {
            // The save on its way decides: it closes the dialog, or says why it failed.
            return;
        }
        look.draft = None;
        look.failed = None;
        look.dialog = false;
        look.sync_hex();
    }

    /// The draft back to classic RAST's wallpaper, keeping the mode and accent.
    pub(crate) fn reset_appearance(&mut self) {
        let reset = self.desktop.look.shown().with_default_wallpaper();
        self.edit_appearance(|look| *look = reset);
    }

    /// Save the draft for this account (Apply, Save changes, or Try again).
    pub(crate) fn save_appearance(&mut self) {
        if self.desktop.look.saving {
            return;
        }
        if parse_hex(&self.desktop.look.hex.text)
            .is_some_and(|typed| typed != self.desktop.look.shown().color)
        {
            self.apply_hex();
        }
        let look = &mut self.desktop.look;
        let Some(draft) = look.draft.clone().filter(|draft| *draft != look.saved) else {
            // Nothing changed: Apply just closes.
            look.draft = None;
            look.failed = None;
            look.dialog = false;
            return;
        };
        if self.session.is_none() {
            // A device login has no server account: the look lasts until the app closes.
            look.saved = draft;
            look.draft = None;
            look.dialog = false;
            return;
        }
        look.saving = true;
        look.failed = None;
        self.calls().save_appearance(&draft);
    }

    /// The taskbar's theme button: light or dark straight away, kept for the account.
    pub(crate) fn toggle_desktop_theme(&mut self) {
        let theme = if self.desktop.is_dark() {
            Theme::Light
        } else {
            Theme::Dark
        };
        let look = &mut self.desktop.look;
        look.saved.theme = theme;
        if let Some(draft) = &mut look.draft {
            draft.theme = theme;
        }
        self.remember_appearance();
        if self.session.is_some() {
            self.calls().save_theme(theme.key());
        }
    }

    /// The account's appearance arrived. Failing to load it keeps the cached or default
    /// look; only an ended session matters.
    pub(crate) fn appearance_loaded(
        &mut self,
        result: Result<Appearance, Failure>,
    ) -> Option<Failure> {
        match result {
            Ok(appearance) => {
                let look = &mut self.desktop.look;
                look.saved = appearance;
                if look.draft.is_none() {
                    look.sync_hex();
                }
                self.remember_appearance();
                None
            }
            Err(failure) => failure.signed_out().then_some(failure),
        }
    }

    /// A save finished: the dialog closes, or the save bar says why it failed.
    pub(crate) fn appearance_saved(
        &mut self,
        result: Result<Appearance, Failure>,
    ) -> Option<Failure> {
        let look = &mut self.desktop.look;
        look.saving = false;
        match result {
            Ok(appearance) => {
                look.saved = appearance;
                look.draft = None;
                look.failed = None;
                look.dialog = false;
                look.sync_hex();
                self.remember_appearance();
                None
            }
            Err(failure) => {
                look.failed = Some(failure.message.clone());
                failure.signed_out().then_some(failure)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_json_round_trips_and_unknown_values_fall_back() {
        let look = Appearance {
            theme: Theme::System,
            accent: Accent::Plum,
            kind: Kind::Color,
            picture: Wallpaper::Desert,
            color: "#2E4756".to_owned(),
            fit: Fit::Contain,
            position: Position::BottomRight,
            overlay: 45,
        };
        assert_eq!(Appearance::from_json(&look.to_json()), Some(look));

        let odd = r##"{"theme":"sepia","accent":"neon","wallpaper":{"kind":"upload",
            "builtinId":"gone","color":"blue","fit":"stretch","position":"middle","overlay":90}}"##;
        let read = Appearance::from_json(odd).unwrap();
        assert_eq!(
            read,
            Appearance {
                overlay: MAX_OVERLAY,
                ..Appearance::default()
            }
        );
    }

    #[test]
    fn reset_keeps_the_mode_and_accent() {
        let look = Appearance {
            theme: Theme::Dark,
            accent: Accent::Rose,
            kind: Kind::Color,
            overlay: 50,
            ..Appearance::default()
        };
        let reset = look.with_default_wallpaper();
        assert_eq!((reset.theme, reset.accent), (Theme::Dark, Accent::Rose));
        assert_eq!(reset.kind, Kind::Picture);
        assert_eq!(reset.overlay, 20);
    }

    #[test]
    fn each_accent_reads_on_its_colour() {
        // Text on the accent keeps a 4.5:1 contrast in both modes (WCAG AA).
        fn contrast(a: Color, b: Color) -> f32 {
            let (hi, lo) = if a.luminance() > b.luminance() {
                (a, b)
            } else {
                (b, a)
            };
            (hi.luminance() + 0.05) / (lo.luminance() + 0.05)
        }
        for accent in Accent::ALL {
            for dark in [false, true] {
                let (fill, ink) = accent.colors(dark);
                assert!(
                    contrast(fill, ink) >= 4.5,
                    "{} {dark}: {}",
                    accent.label(),
                    contrast(fill, ink)
                );
            }
        }
    }
}
