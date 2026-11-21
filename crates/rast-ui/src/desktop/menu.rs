//! The desktop's right-click menu, as classic RAST had it: Personalize, Show desktop,
//! Control Panel, the keyboard shortcuts, About and Sign out.

use rune::{Color, El, Icon, TextStyle, col, el, icon, row, text};

use super::{AppId, Palette, Panel, glyphs};
use crate::State;

const ITEM: TextStyle = TextStyle::new(12.0, 450.0);
const WIDTH: f32 = 220.0;
const ITEM_H: f32 = 30.0;
const SEPARATOR_H: f32 = 9.0;
const PADDING: f32 = 5.0;
/// The menu keeps this far from the window's edges.
const MARGIN: f32 = 8.0;

enum Entry {
    Item {
        key: &'static str,
        glyph: &'static Icon,
        label: &'static str,
        action: fn(&mut State),
    },
    Separator,
    SignOut,
}

fn entries(state: &State) -> Vec<Entry> {
    let mut list = vec![
        Entry::Item {
            key: "desktop-menu-personalize",
            glyph: &glyphs::PICTURE,
            label: "Personalize…",
            action: State::open_personalize,
        },
        Entry::Item {
            key: "desktop-menu-show-desktop",
            glyph: &glyphs::DESKTOP,
            label: "Show desktop",
            action: State::show_desktop,
        },
    ];
    if state.may_open(AppId::Control.access_id()) {
        list.push(Entry::Separator);
        list.push(Entry::Item {
            key: "desktop-menu-control",
            glyph: &glyphs::GEAR,
            label: "Open Control Panel",
            action: |s: &mut State| {
                s.close_desktop_menu();
                s.open_app(AppId::Control);
            },
        });
    }
    list.extend([
        Entry::Separator,
        Entry::Item {
            key: "desktop-menu-shortcuts",
            glyph: &glyphs::KEYBOARD,
            label: "Keyboard shortcuts",
            action: |s: &mut State| {
                s.close_desktop_menu();
                s.toggle_desktop_panel(Panel::Shortcuts);
            },
        },
        Entry::Item {
            key: "desktop-menu-about",
            glyph: &glyphs::INFO,
            label: "About RAST",
            action: |s: &mut State| {
                s.close_desktop_menu();
                s.toggle_desktop_panel(Panel::About);
            },
        },
        Entry::Separator,
        Entry::SignOut,
    ]);
    list
}

/// The menu at `at` (window points), kept inside the window.
pub(super) fn view(state: &State, p: &Palette, at: [f32; 2], [width, height]: [f32; 2]) -> Vec<El> {
    let entries = entries(state);
    let tall = entries.iter().fold(2.0 * PADDING + 2.0, |sum, entry| {
        sum + match entry {
            Entry::Separator => SEPARATOR_H,
            _ => ITEM_H,
        }
    });
    let w = WIDTH.min(width - 2.0 * MARGIN).max(0.0);
    let x = at[0].min(width - w - MARGIN).max(MARGIN.min(width));
    let y = at[1].min(height - tall - MARGIN).max(MARGIN.min(height));
    let mut menu = col()
        .key("desktop-menu")
        .role("menu")
        .label("Desktop")
        .absolute(x.round(), y.round())
        .w(w)
        .padding(PADDING)
        .radius(7.0)
        .border(1.0, p.line)
        .fill(p.surface)
        .shadow(
            if p.dark { 14.0 } else { 12.0 },
            if p.dark { 32.0 } else { 28.0 },
            if p.dark {
                Color::BLACK.alpha(0.5)
            } else {
                super::rgb("#101822").alpha(0.16)
            },
        )
        .modal(true, State::close_desktop_menu);
    for entry in entries {
        menu = menu.child(match entry {
            Entry::Item {
                key,
                glyph,
                label,
                action,
            } => item(p, key, glyph, label, p.ink).on_press(action),
            Entry::Separator => col()
                .w_full()
                .h(SEPARATOR_H)
                .center()
                .a11y_hidden()
                .child(el().w_full().h(1.0).fill(p.subtle)),
            Entry::SignOut => crate::ui::bind_sign_out(
                item(
                    p,
                    "desktop-menu-sign-out",
                    &glyphs::SIGN_OUT,
                    "Sign out",
                    p.danger_text,
                )
                .hover(p.danger.alpha(0.12)),
            ),
        });
    }
    vec![
        el().key("desktop-menu-scrim")
            .role("none")
            .focusable(false)
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .on_press(State::close_desktop_menu),
        menu,
    ]
}

/// A menu entry: its icon and label, `ink` colouring both when it isn't the plain text colour
/// (a danger entry).
pub(super) fn item(
    p: &Palette,
    key: impl Into<String>,
    glyph: &'static Icon,
    label: impl Into<String>,
    ink: Color,
) -> El {
    let tint = if ink == p.ink { p.muted } else { ink };
    entry(key, glyph, label.into(), ink, tint)
        .focusable(true)
        .hover(p.neutral)
}

/// An entry that can't be used now: faint, skipped by focus, and inert.
pub(super) fn dimmed(
    p: &Palette,
    key: impl Into<String>,
    glyph: &'static Icon,
    label: impl Into<String>,
) -> El {
    entry(key, glyph, label.into(), p.faint, p.faint).disabled(true)
}

fn entry(
    key: impl Into<String>,
    glyph: &'static Icon,
    label: String,
    ink: Color,
    tint: Color,
) -> El {
    row()
        .key(key)
        .role("menuitem")
        .label(label.clone())
        .w_full()
        .h(ITEM_H)
        .px(8.0)
        .gap(9.0)
        .center_y()
        .radius(5.0)
        .child(icon(glyph, 16.0).color(tint).a11y_hidden())
        .child(text(label).font(ITEM).color(ink).truncate())
}

/// A popup menu as classic RAST's `.context-menu`: 4 pt inside a hairline border, under a
/// soft shadow, at least 180 pt wide. Place it with `El::anchored`.
pub(super) fn sheet(p: &Palette, key: &'static str, label: &str) -> El {
    col()
        .key(key)
        .role("menu")
        .label(label.to_owned())
        .min_w(180.0)
        .padding(4.0)
        .radius(7.0)
        .border(1.0, p.subtle)
        .fill(p.surface)
        .shadow(
            if p.dark { 14.0 } else { 12.0 },
            if p.dark { 32.0 } else { 28.0 },
            if p.dark {
                Color::BLACK.alpha(0.5)
            } else {
                super::rgb("#101822").alpha(0.16)
            },
        )
}

/// The line between a menu's groups of entries (`.menu-sep`).
pub(super) fn separator(line: Color) -> El {
    el().w_full()
        .h(11.0)
        .px(4.0)
        .center()
        .a11y_hidden()
        .child(el().w_full().h(1.0).fill(line))
}
