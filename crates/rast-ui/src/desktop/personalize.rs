//! Personalize, as classic RAST had it: Mode, Colors, Wallpaper, Solid colour and Layout
//! cards with a save bar. The dialog (Start → Personalize…, the desktop's right-click menu,
//! the account menu) and Control Panel → Appearance edit the one draft the desktop previews.

use rune::{
    Align, Color, Drag, El, Icon, ScrollSpec, SurfaceFit, TextEdit, TextStyle, Tx, col, el, icon,
    row, text, text_input,
};

use super::appearance::{
    Accent, Appearance, Fit, Kind, MAX_OVERLAY, Position, SOLID_COLORS, Status, Theme, hex,
};
use super::{Palette, Wallpaper, glyphs, rgb, wallpaper};
use crate::State;

const TITLE: TextStyle = TextStyle::new(14.0, 650.0);
const PAGE_TITLE: TextStyle = TextStyle::new(18.0, 650.0);
const INTRO: TextStyle = TextStyle::new(12.0, 400.0);
const PAGE_INTRO: TextStyle = TextStyle::new(13.0, 400.0);
const CARD_TITLE: TextStyle = TextStyle::new(12.0, 650.0).tracking(0.24);
const HINT: TextStyle = TextStyle::new(11.5, 400.0);
const OPTION: TextStyle = TextStyle::new(12.0, 550.0);
const SMALL: TextStyle = TextStyle::new(10.5, 400.0).features("'tnum' 1");
const CAPTION: TextStyle = TextStyle::new(11.0, 400.0);
const FIELD: TextStyle = TextStyle::new(11.5, 550.0);
const BUTTON: TextStyle = TextStyle::new(12.0, 560.0);
const STATUS: TextStyle = TextStyle::new(11.0, 450.0);
const VALUE: TextStyle = TextStyle::new(12.0, 400.0).features("'tnum' 1");
const BADGE: TextStyle = TextStyle::new(11.0, 600.0);
const FORM: TextStyle = TextStyle::new(13.0, 400.0);

const DIALOG_INTRO: &str = "Changes preview on the live desktop, including open windows. Apply saves theme, accent and wallpaper to your account; Cancel restores the saved look.";
const PAGE_INTRO_TEXT: &str = "Choose how your desktop looks. Theme, accent and wallpaper are stored with your account, so they follow you to any browser. Changes preview instantly; Save applies them.";

/// The narrowest an accent card and a wallpaper tile get before a row loses a column.
const ACCENT_MIN: f32 = 148.0;
const TILE_MIN: f32 = 150.0;
const TILE_MIN_NARROW: f32 = 120.0;
/// Padding and border inside a card, both sides together.
const CARD_INSET: f32 = 34.0;

/// Where the editor is: the dialog applies and cancels, the page saves and discards.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Dialog,
    Page,
}

// --- the dialog and the page --------------------------------------------------------

/// The Personalize dialog over the whole desktop, taskbar included: the dimmed backdrop,
/// then the dialog in the middle. Escape and a press outside cancel it.
pub(super) fn dialog(state: &State, p: &Palette, [width, height]: [f32; 2]) -> Vec<El> {
    let gutter = if width < 520.0 { 8.0 } else { 20.0 };
    let w = (width - 2.0 * gutter).clamp(0.0, 760.0);
    let tall = (height * 0.86)
        .min(760.0)
        .min(height - 2.0 * gutter)
        .max(0.0);
    let inner = (w - 2.0 - 32.0).max(0.0);
    let head = row()
        .w_full()
        .h(50.0)
        .pl(16.0)
        .pr(10.0)
        .gap(10.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.subtle)
        .child(
            text("Personalize")
                .font(TITLE)
                .color(p.ink)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        )
        .child(close_button(p));
    let body = col()
        .key("personalize-body")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .w_full()
                .px(16.0)
                .pt(14.0)
                .pb(10.0)
                .gap(10.0)
                .child(
                    text(DIALOG_INTRO)
                        .font(INTRO)
                        .color(p.muted)
                        .w_full()
                        .wrap(),
                )
                .child(cards(state, p, inner, w))
                .child(save_bar(state, p, Place::Dialog, inner).sticky_bottom(10.0)),
        );
    let dialog = col()
        .key("personalize")
        .role("dialog")
        .label("Personalize")
        .w(w)
        .h(tall)
        .radius(10.0)
        .clip_rounded(10.0)
        .border(1.0, p.subtle)
        .fill(p.surface)
        .shadow(
            if p.dark { 18.0 } else { 16.0 },
            if p.dark { 44.0 } else { 40.0 },
            if p.dark {
                Color::BLACK.alpha(0.55)
            } else {
                rgb("#101822").alpha(0.18)
            },
        )
        .modal(true, State::discard_appearance)
        .child(head)
        .child(body);
    vec![
        el().key("personalize-scrim")
            .role("none")
            .focusable(false)
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .fill(if p.dark {
                Color::BLACK.alpha(0.55)
            } else {
                rgb("#0e141c").alpha(0.45)
            }),
        row()
            .role("none")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .center()
            .child(dialog),
    ]
}

fn close_button(p: &Palette) -> El {
    el().key("personalize-close")
        .role("button")
        .label("Close")
        .help("Close")
        .focusable(true)
        .size(30.0, 28.0)
        .radius(5.0)
        .center()
        .hover_group()
        .hover(p.danger)
        .child(
            icon(&glyphs::CLOSE, 15.0)
                .color(p.muted)
                .group_color(Color::WHITE)
                .a11y_hidden(),
        )
        .on_press(State::discard_appearance)
}

/// Control Panel → Appearance, `width` points wide in a window `window` points wide: the
/// intro, the save bar (held at the top while the cards scroll) and the cards.
pub(super) fn page(state: &State, p: &Palette, width: f32, window: f32) -> El {
    col()
        .key("appearance-page")
        .w_full()
        .gap(14.0)
        .child(
            col()
                .w_full()
                .gap(6.0)
                .child(text("Appearance").font(PAGE_TITLE).color(p.ink))
                .child(
                    text(PAGE_INTRO_TEXT)
                        .font(PAGE_INTRO)
                        .color(p.muted)
                        .w_full()
                        .wrap(),
                ),
        )
        .child(save_bar(state, p, Place::Page, width).sticky_top(0.0))
        .child(cards(state, p, width, window))
}

// --- the cards ----------------------------------------------------------------------

/// The five cards, `width` points wide; `window` is the dialog's or window's width, which
/// decides where hints go and how small thumbnails get.
fn cards(state: &State, p: &Palette, width: f32, window: f32) -> El {
    let look = state.desktop.look.shown();
    col()
        .w_full()
        .gap(16.0)
        .child(mode_card(p, look.theme, window))
        .child(colors_card(p, look, width, window))
        .child(wallpaper_card(state, p, look, width, window))
        .child(solid_card(state, p, look, window))
        .child(layout_card(p, look, window))
}

/// A card: its title, a hint (on the right, or under the title in narrow windows), and
/// whatever the caller adds.
fn card(
    p: &Palette,
    key: &'static str,
    title: &'static str,
    hint: &'static str,
    window: f32,
) -> El {
    let head = if window <= 620.0 {
        col()
            .w_full()
            .gap(4.0)
            .child(text(title).font(CARD_TITLE).color(p.ink))
            .child(text(hint).font(HINT).color(p.faint).w_full().wrap())
    } else {
        row()
            .w_full()
            .gap(12.0)
            .items_start()
            .child(text(title).font(CARD_TITLE).color(p.ink).grow(1.0))
            .child(
                text(hint)
                    .font(HINT)
                    .color(p.faint)
                    .w_frac(0.55)
                    .wrap()
                    .align(Align::End),
            )
    };
    col()
        .key(key)
        .role("group")
        .label(title)
        .w_full()
        .px(16.0)
        .py(14.0)
        .gap(10.0)
        .radius(10.0)
        .fill(p.surface_2)
        .border(1.0, p.subtle)
        .child(head)
}

/// A line of fine print with an info mark.
fn note(p: &Palette, line: &'static str) -> El {
    row()
        .w_full()
        .gap(7.0)
        .center_y()
        .child(icon(&glyphs::INFO, 14.0).color(p.faint).a11y_hidden())
        .child(
            text(line)
                .font(HINT)
                .color(p.faint)
                .grow(1.0)
                .min_w(0.0)
                .wrap(),
        )
}

fn mode_card(p: &Palette, theme: Theme, window: f32) -> El {
    let items = Theme::ALL.map(|t| {
        let glyph: &'static Icon = match t {
            Theme::Light => &glyphs::SUN,
            Theme::Dark => &glyphs::MOON,
            Theme::System => &glyphs::MONITOR,
        };
        (t, t.key(), t.label(), Some(glyph))
    });
    card(
        p,
        "personalize-mode",
        "Mode",
        "Light, dark, or follow the operating system. The wallpaper and accent stay the same.",
        window,
    )
    .child(segmented(
        p,
        "theme",
        "Colour mode",
        &items,
        theme,
        |s, theme| s.edit_appearance(|look| look.theme = theme),
    ))
    .child(note(
        p,
        "System mode resolves in each browser and follows live OS changes.",
    ))
}

fn colors_card(p: &Palette, look: &Appearance, width: f32, window: f32) -> El {
    let inner = (width - CARD_INSET).max(0.0);
    let (columns, option_w) = columns(inner, ACCENT_MIN, 8.0, Accent::ALL.len());
    let mut grid = col()
        .key("personalize-accents")
        .role("radiogroup")
        .label("Accent colour")
        .w_full()
        .gap(8.0);
    for line in Accent::ALL.chunks(columns) {
        let mut cells = row().w_full().gap(8.0);
        for &accent in line {
            cells = cells.child(accent_option(p, accent, accent == look.accent, option_w));
        }
        grid = grid.child(cells);
    }
    card(
        p,
        "personalize-colors",
        "Colors",
        "Ten presets. Success, warning and error keep their own semantic colours.",
        window,
    )
    .child(grid)
    .child(accent_preview(p, look.accent))
}

/// How many cells of at least `min` fit `width` with `gap` between them (at most `most`),
/// and how wide each is then.
fn columns(width: f32, min: f32, gap: f32, most: usize) -> (usize, f32) {
    let count = (((width + gap) / (min + gap)).floor() as usize).clamp(1, most.max(1));
    let cell = ((width - gap * (count - 1) as f32) / count as f32)
        .floor()
        .max(0.0);
    (count, cell)
}

fn accent_option(p: &Palette, accent: Accent, on: bool, w: f32) -> El {
    let (fill, _) = accent.colors(p.dark);
    let mut swatch = el()
        .size(22.0, 22.0)
        .radius(11.0)
        .fill(fill)
        .border(1.0, Color::BLACK.alpha(0.18))
        .center()
        .a11y_hidden();
    if on {
        swatch = swatch.child(
            el().size(12.0, 12.0)
                .radius(6.0)
                .fill(Color::WHITE.alpha(0.92))
                .border(1.0, Color::BLACK.alpha(0.12)),
        );
    }
    let option = row()
        .key(format!("accent-{}", accent.key()))
        .role("radio")
        .label(accent.label())
        .checked(on)
        .focusable(true)
        .w(w)
        .px(9.0)
        .py(7.0)
        .gap(9.0)
        .center_y()
        .radius(7.0)
        .child(swatch)
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(1.0)
                .a11y_hidden()
                .child(text(accent.label()).font(OPTION).color(p.ink).truncate())
                .child(text(accent.hex(p.dark)).font(SMALL).color(p.faint)),
        )
        .on_press(move |s: &mut State| s.edit_appearance(|look| look.accent = accent));
    if on {
        option.fill(p.hover).border(1.0, p.accent_line)
    } else {
        option
            .fill(p.surface)
            .border(1.0, p.subtle)
            .hover(p.neutral)
    }
}

/// The accent at work: a primary button, a link, a selection and a field.
fn accent_preview(p: &Palette, accent: Accent) -> El {
    row()
        .key("personalize-accent-preview")
        .a11y_hidden()
        .w_full()
        .flex_wrap()
        .gap(10.0)
        .center_y()
        .px(12.0)
        .py(10.0)
        .radius(7.0)
        .border(1.0, p.subtle)
        .fill(p.surface)
        .child(
            row()
                .h(27.0)
                .px(10.0)
                .center()
                .radius(5.0)
                .fill(p.accent)
                .child(text("Primary").font(BUTTON).color(p.accent_ink)),
        )
        .child(
            col()
                .gap(1.0)
                .child(text("A link").font(OPTION).color(p.accent_text))
                .child(el().w_full().h(1.0).fill(p.accent_text)),
        )
        .child(
            el().px(8.0)
                .py(3.0)
                .radius(5.0)
                .fill(p.hover)
                .child(text("Selected").font(BADGE).color(p.accent_text)),
        )
        .child(
            row()
                .w(140.0)
                .h(32.0)
                .px(10.0)
                .center_y()
                .radius(5.0)
                .border(1.0, p.line)
                .fill(p.surface)
                .child(text("Form field").font(FORM).color(p.ink)),
        )
        .child(text(accent.hex(p.dark)).font(INTRO).color(p.faint))
}

fn wallpaper_card(state: &State, p: &Palette, look: &Appearance, width: f32, window: f32) -> El {
    let inner = (width - CARD_INSET).max(0.0);
    let min = if window <= 560.0 {
        TILE_MIN_NARROW
    } else {
        TILE_MIN
    };
    let (columns, tile_w) = columns(inner, min, 10.0, Wallpaper::ALL.len() + 1);
    let media_h = (tile_w * 9.0 / 16.0).round();
    let mut tiles: Vec<El> = Wallpaper::ALL
        .iter()
        .map(|&choice| {
            let mut media = el().size(tile_w, media_h).fill(p.surface_3);
            if let Some(image) = wallpaper(state, choice) {
                media = media.pixels_fit(image.pixels(), SurfaceFit::Fill);
            }
            tile(
                p,
                format!("wallpaper-{}", choice.key()),
                choice.label(),
                choice.caption(),
                media,
                look.kind == Kind::Picture && look.picture == choice,
                tile_w,
            )
            .on_press(move |s: &mut State| {
                s.edit_appearance(|look| {
                    look.kind = Kind::Picture;
                    look.picture = choice;
                });
            })
        })
        .collect();
    tiles.push(
        tile(
            p,
            "wallpaper-color".to_owned(),
            "Solid colour",
            "A calm single colour, no image",
            el().size(tile_w, media_h).fill(hex(&look.color)),
            look.kind == Kind::Color,
            tile_w,
        )
        .on_press(|s: &mut State| s.edit_appearance(|look| look.kind = Kind::Color)),
    );
    let mut gallery = col()
        .key("personalize-wallpapers")
        .role("radiogroup")
        .label("Wallpaper")
        .w_full()
        .gap(12.0);
    let mut tiles = tiles.into_iter().peekable();
    while tiles.peek().is_some() {
        let mut line = row().w_full().gap(10.0).items_start();
        for tile in tiles.by_ref().take(columns) {
            line = line.child(tile);
        }
        gallery = gallery.child(line);
    }
    card(
        p,
        "personalize-wallpaper",
        "Wallpaper",
        "Bundled nature scenes are stored with the application and need no upload.",
        window,
    )
    .child(gallery)
}

/// A wallpaper choice: its picture (or colour), name and a line about it.
fn tile(
    p: &Palette,
    key: String,
    label: &'static str,
    caption: &'static str,
    media: El,
    on: bool,
    w: f32,
) -> El {
    let mut frame = media
        .radius(5.0)
        .clip_rounded(5.0)
        .border(
            if on { 2.0 } else { 1.0 },
            if on { p.accent } else { p.subtle },
        )
        .a11y_hidden();
    if on {
        frame = frame.child(
            el().size(20.0, 20.0)
                .radius(10.0)
                .fill(p.accent)
                .center()
                .pin(None, None, Some(6.0), Some(6.0))
                .child(icon(&glyphs::CHECK, 12.0).color(p.accent_ink)),
        );
    }
    col()
        .key(key)
        .role("radio")
        .label(label)
        .checked(on)
        .focusable(true)
        .w(w)
        .gap(6.0)
        .child(frame)
        .child(
            text(label)
                .font(OPTION)
                .color(p.ink)
                .truncate()
                .a11y_hidden(),
        )
        .child(
            text(caption)
                .font(CAPTION)
                .color(p.faint)
                .w_full()
                .wrap()
                .a11y_hidden(),
        )
}

fn hex_model(s: &mut State) -> &mut TextEdit {
    &mut s.desktop.look.hex
}

fn solid_card(state: &State, p: &Palette, look: &Appearance, window: f32) -> El {
    let well = el()
        .key("personalize-color-well")
        .focusable(false)
        .size(40.0, 32.0)
        .radius(5.0)
        .border(1.0, p.line)
        .fill(p.surface)
        .padding(3.0)
        .a11y_hidden()
        .child(el().w_full().h_full().radius(3.0).fill(hex(&look.color)))
        .on_press_tx(|_: &mut State, tx: &mut Tx| tx.focus("personalize-hex", true));
    let field = text_input(&state.desktop.look.hex, VALUE)
        .key("personalize-hex")
        .label("Wallpaper colour, as #RRGGBB")
        .single_line()
        .w(96.0)
        .h(32.0)
        .px(9.0)
        .py(7.0)
        .radius(5.0)
        .fill(p.surface)
        .border(1.0, p.line)
        .color(p.ink)
        .placeholder("#RRGGBB", p.faint)
        .on_edit(hex_model)
        .on_submit(|s: &mut State, _: &mut Tx| s.apply_hex());
    let mut swatches = row()
        .key("personalize-colors-solid")
        .role("radiogroup")
        .label("Solid colours")
        .flex_wrap()
        .gap(2.0);
    for color in SOLID_COLORS {
        let on = look.kind == Kind::Color && look.color == color;
        swatches = swatches.child(
            el().key(format!("color-{}", &color[1..]))
                .role("radio")
                .label(format!("Use colour {color}"))
                .help(color)
                .checked(on)
                .focusable(true)
                .size(30.0, 30.0)
                .radius(7.0)
                .center()
                .border(2.0, if on { p.accent } else { Color::TRANSPARENT })
                .child(
                    el().size(24.0, 24.0)
                        .radius(5.0)
                        .fill(hex(color))
                        .border(1.0, Color::BLACK.alpha(0.15)),
                )
                .on_press(move |s: &mut State| {
                    s.edit_appearance(|look| {
                        look.kind = Kind::Color;
                        color.clone_into(&mut look.color);
                    });
                }),
        );
    }
    card(
        p,
        "personalize-solid",
        "Solid colour",
        "Useful on low-power displays or for a distraction-free desktop.",
        window,
    )
    .child(
        row()
            .w_full()
            .flex_wrap()
            .gap(10.0)
            .center_y()
            .child(well)
            .child(field)
            .child(swatches),
    )
}

fn layout_card(p: &Palette, look: &Appearance, window: f32) -> El {
    let fits = Fit::ALL.map(|fit| (fit, fit.key(), fit.label(), None));
    let fit = field(
        p,
        "Fit",
        segmented(p, "fit", "Image fit", &fits, look.fit, |s, fit| {
            s.edit_appearance(|look| look.fit = fit);
        }),
    );
    let mut grid = col()
        .key("personalize-position")
        .role("radiogroup")
        .label("Image position")
        .gap(4.0);
    for line in Position::ALL.chunks(3) {
        let mut cells = row().gap(4.0);
        for &position in line {
            let on = look.position == position;
            let cell = el()
                .key(format!("position-{}", position.key()))
                .role("radio")
                .label(format!("Position {}", position.label()))
                .help(position.label())
                .checked(on)
                .focusable(true)
                .size(30.0, 24.0)
                .radius(5.0)
                .center()
                .border(1.0, if on { p.accent } else { p.line })
                .fill(if on { p.hover } else { p.surface })
                .child(
                    el().size(6.0, 6.0)
                        .radius(3.0)
                        .fill(if on { p.accent } else { p.faint }),
                )
                .on_press(move |s: &mut State| s.edit_appearance(|look| look.position = position));
            cells = cells.child(if on { cell } else { cell.hover(p.surface_2) });
        }
        grid = grid.child(cells);
    }
    let position = field(p, "Position", grid);
    let overlay = field(p, "Dark overlay", slider(p, look.overlay))
        .grow(1.0)
        .min_w(220.0);
    card(
        p,
        "personalize-layout",
        "Layout",
        "Control how the image fills the desktop and add a little darkening for legibility.",
        window,
    )
    .child(
        row()
            .w_full()
            .flex_wrap()
            .gap(12.0)
            .items_start()
            .child(fit)
            .child(position)
            .child(overlay),
    )
    .child(note(
        p,
        "Position applies to Cover and Contain; Center keeps the image at its natural size.",
    ))
}

/// A labelled control in the Layout card.
fn field(p: &Palette, label: &'static str, control: El) -> El {
    col()
        .gap(5.0)
        .min_w(150.0)
        .child(text(label).font(FIELD).color(p.muted))
        .child(control)
}

/// One choice of a few, the selected one filled with the accent.
fn segmented<T: Copy + PartialEq + 'static>(
    p: &Palette,
    key: &'static str,
    label: &'static str,
    items: &[(T, &'static str, &'static str, Option<&'static Icon>)],
    selected: T,
    pick: fn(&mut State, T),
) -> El {
    let mut group = row()
        .key(format!("personalize-{key}"))
        .role("radiogroup")
        .label(label)
        .self_start()
        .flex_wrap()
        .gap(2.0)
        .padding(3.0)
        .radius(7.0)
        .fill(p.surface_2)
        .border(1.0, p.subtle);
    for &(value, suffix, name, glyph) in items {
        let on = value == selected;
        let ink = if on { p.accent_ink } else { p.muted };
        let mut item = row()
            .key(format!("{key}-{suffix}"))
            .role("radio")
            .label(name)
            .checked(on)
            .focusable(true)
            .h(26.0)
            .px(10.0)
            .gap(5.0)
            .center()
            .radius(5.0);
        if let Some(glyph) = glyph {
            item = item.child(icon(glyph, 14.0).color(ink).a11y_hidden());
        }
        item = item.child(text(name).font(OPTION).color(ink).a11y_hidden());
        item = if on {
            item.fill(p.accent).hover(p.accent_hover)
        } else {
            item.hover(p.neutral)
        };
        group = group.child(item.on_press(move |s: &mut State| pick(s, value)));
    }
    group
}

/// The dark overlay, 0 to 60 % in steps of 5: drag along it, or use the arrow keys.
fn slider(p: &Palette, value: u8) -> El {
    let fraction = f32::from(value) / f32::from(MAX_OVERLAY);
    let track = row()
        .key("personalize-overlay")
        .role("slider")
        .label("Dark overlay")
        .range_value(f64::from(value), 0.0, f64::from(MAX_OVERLAY))
        .a11y_value(format!("{value}%"))
        .focusable(true)
        .grow(1.0)
        .min_w(120.0)
        .h(22.0)
        .center_y()
        .child(
            el().pin(Some(8.0), Some(9.0), Some(8.0), None)
                .h(4.0)
                .radius(2.0)
                .fill(p.line),
        )
        .child(
            el().grow(fraction)
                .h(4.0)
                .ml(8.0)
                .mr(-8.0)
                .radius(2.0)
                .fill(p.accent),
        )
        .child(
            el().size(16.0, 16.0)
                .radius(8.0)
                .fill(p.accent)
                .border(2.0, p.surface),
        )
        .child(el().grow(1.0 - fraction))
        .on_drag(|s: &mut State, d: &mut Drag, _: &mut Tx| {
            let span = (d.w - 16.0).max(1.0);
            let share = ((d.x - 8.0) / span).clamp(0.0, 1.0);
            let step = (share * f32::from(MAX_OVERLAY) / 5.0).round() as i32;
            set_overlay(s, step * 5);
        })
        .on_step(|s: &mut State, delta: i32, _: &mut Tx| {
            let now = i32::from(s.desktop.look.shown().overlay);
            set_overlay(s, now + 5 * delta);
        })
        .on_step_limit(|s: &mut State, max: bool, _: &mut Tx| {
            set_overlay(s, if max { i32::from(MAX_OVERLAY) } else { 0 });
        });
    row().w_full().gap(10.0).center_y().child(track).child(
        text(format!("{value}%"))
            .font(VALUE)
            .color(p.muted)
            .w(38.0)
            .align(Align::End)
            .a11y_hidden(),
    )
}

fn set_overlay(s: &mut State, percent: i32) {
    let percent = percent.clamp(0, i32::from(MAX_OVERLAY)) as u8;
    if s.desktop.look.shown().overlay != percent {
        s.edit_appearance(|look| look.overlay = percent);
    }
}

// --- the save bar -------------------------------------------------------------------

/// Reset to default and Cancel (the dialog) or Discard changes (the page), what has been
/// saved, and Apply or Save changes. In a narrow bar the status and the save button take a
/// second line.
fn save_bar(state: &State, p: &Palette, place: Place, width: f32) -> El {
    let look = &state.desktop.look;
    let status = look.status();
    let saving = status == Status::Saving;
    let (save, busy) = match place {
        Place::Dialog => ("Apply", "Applying…"),
        Place::Page => ("Save changes", "Saving…"),
    };
    let reset = button(
        p,
        "personalize-reset",
        "Reset to default",
        Some(&glyphs::PICTURE),
        Look::Plain,
        (!saving).then_some(State::reset_appearance as fn(&mut State)),
    );
    let second = match place {
        Place::Dialog => button(
            p,
            "personalize-cancel",
            "Cancel",
            None,
            Look::Plain,
            (!saving).then_some(State::discard_appearance as fn(&mut State)),
        ),
        Place::Page => button(
            p,
            "personalize-discard",
            "Discard changes",
            Some(&glyphs::UNDO),
            Look::Plain,
            matches!(status, Status::Dirty | Status::Failed(_))
                .then_some(State::discard_appearance as fn(&mut State)),
        ),
    };
    let (said, said_color) = match status {
        Status::Clean => ("All changes saved", p.ok_text),
        Status::Dirty => ("Unsaved changes", p.warn_text),
        Status::Saving => (busy, p.faint),
        Status::Failed(_) => ("Could not save", p.danger_text),
    };
    let save_action = Some(State::save_appearance as fn(&mut State));
    let commit = match status {
        Status::Clean => button(p, "personalize-save", save, None, Look::Plain, None),
        Status::Dirty => button(
            p,
            "personalize-save",
            save,
            Some(&glyphs::CHECK),
            Look::Primary,
            save_action,
        ),
        Status::Saving => button(p, "personalize-save", busy, None, Look::Primary, None),
        Status::Failed(_) => button(
            p,
            "personalize-save",
            "Try again",
            Some(&glyphs::UNDO),
            Look::Primary,
            save_action,
        ),
    };
    let said = text(said)
        .key("personalize-status")
        .font(STATUS)
        .color(said_color)
        .truncate();
    let leading = row()
        .flex_wrap()
        .gap(6.0)
        .center_y()
        .child(reset)
        .child(second);
    let actions = if width < 560.0 {
        col().w_full().gap(8.0).child(leading).child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(said.grow(1.0).min_w(0.0))
                .child(commit),
        )
    } else {
        row()
            .w_full()
            .gap(8.0)
            .center_y()
            .child(leading)
            .child(el().grow(1.0))
            .child(said.min_w(0.0).mr(4.0))
            .child(commit)
    };
    let mut bar = col()
        .key("personalize-bar")
        .w_full()
        .gap(8.0)
        .px(10.0)
        .py(8.0)
        .radius(10.0)
        .border(1.0, p.subtle)
        .fill(p.surface)
        .shadow(
            1.0,
            2.0,
            if p.dark {
                Color::BLACK.alpha(0.35)
            } else {
                rgb("#141e2a").alpha(0.08)
            },
        );
    if let Status::Failed(reason) = status {
        bar = bar.child(
            text(reason.to_owned())
                .key("personalize-error")
                .role("alert")
                .font(INTRO)
                .color(p.danger_text)
                .w_full()
                .wrap()
                .px(10.0)
                .py(7.0)
                .radius(7.0)
                .fill(p.danger.alpha(0.1))
                .border(1.0, p.danger.alpha(0.3)),
        );
    }
    bar.child(actions)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Primary,
    Plain,
}

/// A classic RAST button; without an action it is inactive (muted, unfilled for Plain).
fn button(
    p: &Palette,
    key: &'static str,
    label: &'static str,
    glyph: Option<&'static Icon>,
    look: Look,
    action: Option<fn(&mut State)>,
) -> El {
    let active = action.is_some();
    let (fill, edge, ink, hover) = match look {
        Look::Primary => (p.accent, p.accent, p.accent_ink, Some(p.accent_hover)),
        Look::Plain if active => (p.surface, p.line, p.ink, Some(p.neutral)),
        Look::Plain => (p.surface, p.subtle, p.faint, None),
    };
    let mut item = row()
        .key(key)
        .role("button")
        .label(label)
        .focusable(true)
        .disabled(!active)
        .h(32.0)
        .px(12.0)
        .gap(6.0)
        .center()
        .radius(5.0)
        .fill(fill)
        .border(1.0, edge);
    if let Some(glyph) = glyph {
        item = item.child(icon(glyph, 15.0).color(ink).a11y_hidden());
    }
    item = item.child(text(label).font(BUTTON).color(ink).a11y_hidden());
    match (action, hover) {
        (Some(action), Some(hover)) => item.hover(hover).on_press(action),
        (Some(action), None) => item.on_press(action),
        (None, _) => item,
    }
}

#[cfg(test)]
mod tests {
    use rune::{Env, Key, KeyEvent, Runtime};

    use super::super::AppId;
    use super::super::appearance::{Accent, Appearance, Kind, Status, Theme};
    use crate::server::{Failure, Reply};
    use crate::{Config, SIZE, Screen, State, ui};

    /// Signed in to the server as root, on the desktop.
    fn desktop(size: (f32, f32)) -> Runtime<State> {
        let mut state = State::new(Config::default());
        state.persist = false;
        state.session = crate::sample_accounts().into_iter().next();
        state.signed_in = true;
        state.screen = Screen::Desktop;
        let env = Env {
            system_dark: false,
            scale: 2.0,
        };
        let mut rt = Runtime::new(state, ui::view, size, env);
        settle(&mut rt);
        rt
    }

    fn settle(rt: &mut Runtime<State>) {
        rt.invalidate();
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
    }

    fn press(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt
            .find(key)
            .unwrap_or_else(|| panic!("`{key}` is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        settle(rt);
    }

    fn right_click(rt: &mut Runtime<State>, x: f32, y: f32) {
        rt.secondary_down(x, y);
        rt.secondary_up(x, y);
        settle(rt);
    }

    fn rect(rt: &Runtime<State>, key: &str) -> [f32; 4] {
        rt.presented(key)
            .unwrap_or_else(|| panic!("`{key}` is drawn"))
            .0
    }

    /// Every call sent since the last drain.
    fn sent(state: &State) -> Vec<Reply> {
        std::iter::from_fn(|| state.inbox.rx.try_recv().ok()).collect()
    }

    /// Scroll the dialog's cards to the end.
    fn scroll_down(rt: &mut Runtime<State>) {
        let (x, y) = rt.find("personalize-body").expect("the dialog is open");
        rt.wheel(x, y, -10_000.0);
        settle(rt);
    }

    #[test]
    fn personalize_previews_the_draft_and_cancel_puts_back_the_saved_look() {
        let mut rt = desktop(SIZE);
        assert!(!rt.state.desktop.is_dark(), "classic RAST starts light");
        press(&mut rt, "desktop-start");
        press(&mut rt, "start-personalize");
        assert!(rt.state.desktop.look.dialog && rt.find("personalize").is_some());

        press(&mut rt, "theme-dark");
        press(&mut rt, "accent-plum");
        let look = &rt.state.desktop.look;
        assert!(rt.state.desktop.is_dark(), "the desktop previews the draft");
        assert_eq!(look.shown().accent, Accent::Plum);
        assert_eq!(look.saved, Appearance::default(), "nothing is saved yet");
        assert_eq!(look.status(), Status::Dirty);

        press(&mut rt, "personalize-cancel");
        assert!(!rt.state.desktop.look.dialog, "Cancel closes the dialog");
        assert_eq!(*rt.state.desktop.look.shown(), Appearance::default());
        assert!(!rt.state.desktop.is_dark());

        rt.update(|s, _| s.open_personalize());
        settle(&mut rt);
        press(&mut rt, "accent-teal");
        rt.key(KeyEvent::from(Key::Escape));
        settle(&mut rt);
        assert!(!rt.state.desktop.look.dialog, "Escape cancels too");
        assert_eq!(rt.state.desktop.look.shown().accent, Accent::Ocean);
    }

    #[test]
    fn apply_saves_for_the_account_and_a_refusal_says_why() {
        let mut rt = desktop(SIZE);
        rt.update(|s, _| s.open_personalize());
        settle(&mut rt);
        let _ = sent(&rt.state);
        press(&mut rt, "accent-forest");
        press(&mut rt, "wallpaper-desert-dusk");
        press(&mut rt, "personalize-save");
        assert_eq!(rt.state.desktop.look.status(), Status::Saving);
        assert!(
            sent(&rt.state)
                .iter()
                .any(|reply| matches!(reply, Reply::AppearanceSaved(_))),
            "Apply sends the look"
        );

        rt.update(|s, tx| {
            let refused = Failure {
                status: 400,
                message: "Choose a colour: ocean, forest.".to_owned(),
            };
            s.apply_reply(Reply::AppearanceSaved(Err(refused)), tx);
        });
        settle(&mut rt);
        let look = &rt.state.desktop.look;
        assert!(look.dialog, "the dialog stays to try again");
        assert_eq!(
            look.status(),
            Status::Failed("Choose a colour: ocean, forest.")
        );
        assert!(rt.find("personalize-error").is_some());
        assert_eq!(look.shown().accent, Accent::Forest, "the draft is kept");

        press(&mut rt, "personalize-save");
        let saved = rt.state.desktop.look.shown().clone();
        rt.update(|s, tx| s.apply_reply(Reply::AppearanceSaved(Ok(saved)), tx));
        settle(&mut rt);
        let look = &rt.state.desktop.look;
        assert!(!look.dialog, "a saved look closes the dialog");
        assert_eq!(look.saved.accent, Accent::Forest);
        assert_eq!(look.saved.picture, super::super::Wallpaper::Desert);
        assert_eq!(look.status(), Status::Clean);
    }

    #[test]
    fn the_desktop_menu_opens_where_right_clicked() {
        let mut rt = desktop(SIZE);
        rt.update(|s, _| s.open_app(AppId::Control));
        settle(&mut rt);
        right_click(&mut rt, 1200.0, 300.0);
        assert_eq!(rt.state.desktop.menu, Some([1200.0, 300.0]));
        let [x, y, w, h] = rect(&rt, "desktop-menu");
        assert!(
            x + w <= SIZE.0 - 7.5 && y + h <= SIZE.1 - 7.5,
            "kept inside the window"
        );
        assert!(y <= 300.5, "opens at the pointer");

        press(&mut rt, "desktop-menu-show-desktop");
        assert_eq!(rt.state.desktop.menu, None);
        assert!(
            rt.state.desktop.windows.all().iter().all(|w| w.minimized),
            "Show desktop minimizes every window"
        );

        right_click(&mut rt, 640.0, 500.0);
        let (x, y) = (300.0, 120.0);
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        settle(&mut rt);
        assert_eq!(rt.state.desktop.menu, None, "a press outside closes it");

        right_click(&mut rt, 640.0, 500.0);
        press(&mut rt, "desktop-menu-personalize");
        assert!(
            rt.state.desktop.look.dialog,
            "Personalize… opens the dialog"
        );
        assert_eq!(rt.state.desktop.menu, None);
    }

    #[test]
    fn control_panel_appearance_previews_saves_and_discards() {
        let mut rt = desktop(SIZE);
        rt.update(|s, _| s.open_app(AppId::Control));
        settle(&mut rt);
        press(&mut rt, "control-tab-appearance");
        assert!(rt.find("appearance-page").is_some());
        assert_eq!(rt.state.desktop.look.status(), Status::Clean);

        press(&mut rt, "theme-system");
        assert_eq!(rt.state.desktop.look.shown().theme, Theme::System);
        assert!(!rt.state.desktop.is_dark());
        rt.set_system_dark(true);
        settle(&mut rt);
        assert!(
            rt.state.desktop.is_dark(),
            "System follows the operating system"
        );

        press(&mut rt, "personalize-discard");
        assert_eq!(*rt.state.desktop.look.shown(), Appearance::default());

        press(&mut rt, "accent-rose");
        let _ = sent(&rt.state);
        press(&mut rt, "personalize-save");
        assert!(
            sent(&rt.state)
                .iter()
                .any(|reply| matches!(reply, Reply::AppearanceSaved(_)))
        );
        let saved = rt.state.desktop.look.shown().clone();
        rt.update(|s, tx| s.apply_reply(Reply::AppearanceSaved(Ok(saved)), tx));
        settle(&mut rt);
        assert_eq!(rt.state.desktop.look.saved.accent, Accent::Rose);

        press(&mut rt, "accent-amber");
        press(&mut rt, "control-tab-profile");
        assert_eq!(
            rt.state.desktop.look.shown().accent,
            Accent::Rose,
            "leaving the page puts back the saved look"
        );
    }

    #[test]
    fn the_overlay_and_solid_colours_follow_the_pointer_and_keys() {
        let mut rt = desktop(SIZE);
        rt.update(|s, _| s.open_personalize());
        settle(&mut rt);
        scroll_down(&mut rt);

        let [x, y, w, h] = rect(&rt, "personalize-overlay");
        let cy = y + h / 2.0;
        rt.pointer_down(x + 8.0, cy);
        rt.pointer_up(x + 8.0, cy);
        settle(&mut rt);
        assert_eq!(rt.state.desktop.look.shown().overlay, 0);
        rt.pointer_down(x + 8.0, cy);
        rt.pointer_move(x + 8.0 + (w - 16.0) * 0.52, cy);
        rt.pointer_up(x + 8.0 + (w - 16.0) * 0.52, cy);
        settle(&mut rt);
        assert_eq!(rt.state.desktop.look.shown().overlay, 30, "steps of 5");
        rt.key(KeyEvent::from(Key::Right));
        settle(&mut rt);
        assert_eq!(rt.state.desktop.look.shown().overlay, 35);
        rt.key(KeyEvent::from(Key::End));
        settle(&mut rt);
        assert_eq!(rt.state.desktop.look.shown().overlay, 60);

        press(&mut rt, "color-2E4756");
        let look = rt.state.desktop.look.shown();
        assert_eq!((look.kind, look.color.as_str()), (Kind::Color, "#2E4756"));
        assert_eq!(rt.state.desktop.look.hex.text, "#2E4756");

        rt.update(|s, _| {
            s.desktop.look.hex = rune::TextEdit::new("#a1b2c3");
            s.apply_hex();
        });
        assert_eq!(rt.state.desktop.look.shown().color, "#A1B2C3");
        rt.update(|s, _| {
            s.desktop.look.hex = rune::TextEdit::new("blue");
            s.apply_hex();
        });
        assert_eq!(
            rt.state.desktop.look.shown().color,
            "#A1B2C3",
            "not a colour"
        );
        assert_eq!(rt.state.desktop.look.hex.text, "#A1B2C3");
    }

    #[test]
    fn personalize_fits_a_phone() {
        let (width, height) = (390.0, 844.0);
        let mut rt = desktop((width, height));
        rt.update(|s, _| s.open_personalize());
        settle(&mut rt);
        let [x, y, w, h] = rect(&rt, "personalize");
        assert!(x >= 0.0 && x + w <= width && y >= 0.0 && y + h <= height);
        for key in [
            "personalize-reset",
            "personalize-cancel",
            "personalize-save",
            "accent-ocean",
            "accent-sage",
            "wallpaper-alpine-dawn",
        ] {
            let [kx, _, kw, _] = rect(&rt, key);
            assert!(kx >= x && kx + kw <= x + w + 0.5, "{key} fits the dialog");
        }
    }

    #[test]
    fn the_taskbar_theme_button_keeps_only_the_mode() {
        let mut rt = desktop(SIZE);
        let _ = sent(&rt.state);
        press(&mut rt, "desktop-theme");
        assert!(rt.state.desktop.is_dark());
        assert_eq!(rt.state.desktop.look.saved.theme, Theme::Dark);
        assert!(
            sent(&rt.state)
                .iter()
                .any(|reply| matches!(reply, Reply::Appearance(_))),
            "the mode is saved for the account"
        );
        let mut look = Appearance {
            theme: Theme::Dark,
            ..Appearance::default()
        };
        look.accent = Accent::Indigo;
        rt.update(|s, tx| s.apply_reply(Reply::Appearance(Ok(look)), tx));
        assert_eq!(rt.state.desktop.look.saved.accent, Accent::Indigo);
    }
}
