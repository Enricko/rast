//! Control Panel's frame, as classic RAST drew it: the sections down the left (an icon rail
//! in a narrow window), and a header that names the open section, with its action, above
//! the page, which scrolls under it.

use rune::{Align, El, TextStyle, col, el, icon, row, text};

use super::{Tab, tabs};
use crate::State;
use crate::desktop::{Palette, glyphs};

const ITEM: TextStyle = TextStyle::new(13.0, 400.0);
const NOTE: TextStyle = TextStyle::new(12.0, 400.0).lh(16.8);
const TITLE: TextStyle = TextStyle::new(16.0, 680.0).tracking(-0.16);
const TITLE_SMALL: TextStyle = TextStyle::new(15.0, 680.0).tracking(-0.15);
const DETAIL: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const ACTION: TextStyle = TextStyle::new(12.0, 560.0);
const INTRO_TITLE: TextStyle = TextStyle::new(18.0, 690.0).tracking(-0.27);
const INTRO: TextStyle = TextStyle::new(12.8, 400.0).lh(19.2);
const SEGMENT: TextStyle = TextStyle::new(12.0, 550.0);
const HINT: TextStyle = TextStyle::new(12.0, 400.0).lh(17.4);
const EMPTY_TITLE: TextStyle = TextStyle::new(16.0, 660.0);
const EMPTY_DETAIL: TextStyle = TextStyle::new(13.0, 400.0).lh(19.5);

/// The sidebar's width.
const NAV: f32 = 214.0;
/// In a window this wide or narrower the sidebar is an icon rail [`RAIL`] wide, and the
/// header and page take smaller margins.
const NARROW: f32 = 820.0;
const RAIL: f32 = 56.0;
/// In a window this narrow the margins shrink once more and the title steps down.
const TINY: f32 = 480.0;

pub(super) fn narrow(window: f32) -> bool {
    window <= NARROW
}

/// The sidebar's width in a window `window` points wide (its border draws inside it).
pub(super) fn nav_width(window: f32) -> f32 {
    if narrow(window) { RAIL } else { NAV }
}

/// The page's side margins in a window `window` points wide.
pub(super) fn margin(window: f32) -> f32 {
    if window <= TINY {
        12.0
    } else if narrow(window) {
        14.0
    } else {
        20.0
    }
}

/// The sections down the left. The open one is tinted with the accent; the others take
/// a neutral tint under the pointer.
pub(super) fn nav(state: &State, p: &Palette, open: Tab, window: f32) -> El {
    let rail = narrow(window);
    let mut list = col().w_full().gap(2.0);
    for section in tabs(state) {
        list = list.child(nav_item(p, section, section == open.section(), rail));
    }
    let mut nav = col()
        .key("control-nav")
        .role("tablist")
        .label("Control Panel sections")
        .w(nav_width(window))
        .px(if rail { 7.0 } else { 8.0 })
        .py(if rail { 10.0 } else { 12.0 })
        .gap(8.0)
        .fill(p.surface_2)
        .border_sides([0.0, 1.0, 0.0, 0.0], p.subtle)
        .child(list);
    if !rail && state.session.as_ref().is_some_and(|me| !me.role.is_admin()) {
        nav = nav.child(el().grow(1.0)).child(
            text("Administrator sections are hidden for this account.")
                .key("control-nav-note")
                .font(NOTE)
                .color(p.faint)
                .w_full()
                .padding(8.0)
                .wrap(),
        );
    }
    nav
}

fn nav_item(p: &Palette, section: Tab, open: bool, rail: bool) -> El {
    let label = section.label();
    let (glyph, ink) = if open {
        (p.accent, p.accent_text)
    } else {
        (p.faint, p.muted)
    };
    let mut item = row()
        .key(section.key())
        .role("tab")
        .label(label)
        .checked(open)
        .focusable(true)
        .w_full()
        .radius(5.0)
        .hover_group()
        .child(
            icon(section.glyph(), 17.0)
                .color(glyph)
                .group_color(if open { glyph } else { p.ink })
                .a11y_hidden(),
        )
        .on_press(move |s: &mut State| s.open_control_tab(section));
    item = if rail {
        // The name stays the accessible label and shows as a tooltip.
        item.h(40.0).center().help(label)
    } else {
        item.h(36.0).px(10.0).gap(9.0).center_y().child(
            text(label)
                .font(ITEM)
                .color(ink)
                .group_color(if open { ink } else { p.ink })
                .truncate()
                .a11y_hidden(),
        )
    };
    if open {
        item.fill(p.hover).border(1.0, p.accent_line)
    } else {
        item.hover(p.neutral)
    }
}

/// The header above the page: the section's name, a line about it, and its action.
pub(super) fn header(p: &Palette, section: Tab, action: Option<El>, window: f32) -> El {
    let narrow = narrow(window);
    let mut words = col().grow(1.0).min_w(120.0).gap(2.0).child(
        text(section.label())
            .font(if window <= TINY { TITLE_SMALL } else { TITLE })
            .color(p.ink),
    );
    let detail = section.detail();
    if !detail.is_empty() {
        words = words.child(text(detail).font(DETAIL).color(p.muted).w_full().wrap());
    }
    let mut header = row()
        .key("control-header")
        .w_full()
        .px(if narrow { 14.0 } else { 20.0 })
        .pt(if narrow { 11.0 } else { 13.0 })
        .pb(if narrow { 12.0 } else { 14.0 })
        .gap(12.0)
        .center_y()
        .flex_wrap()
        .fill(p.surface)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.subtle)
        .child(words);
    if let Some(action) = action {
        header = header.child(action);
    }
    header
}

/// The header's primary action, with a leading plus: *Add user*, *Create group*.
pub(super) fn action(
    p: &Palette,
    key: &'static str,
    label: &'static str,
    press: fn(&mut State),
) -> El {
    row()
        .key(key)
        .role("button")
        .label(label)
        .focusable(true)
        .h(32.0)
        .px(12.0)
        .gap(6.0)
        .center()
        .radius(5.0)
        .fill(p.accent)
        .hover(p.accent_hover)
        .child(icon(&glyphs::PLUS, 14.0).color(p.accent_ink).a11y_hidden())
        .child(text(label).font(ACTION).color(p.accent_ink).a11y_hidden())
        .on_press(press)
}

/// A page's lead-in: its own heading (none when the header's name says it all) and a
/// line about what it does.
pub(super) fn intro(p: &Palette, title: Option<&'static str>, detail: &'static str) -> El {
    let mut intro = col().w_full().max_w(760.0).gap(4.0).pb(4.0);
    if let Some(title) = title {
        intro = intro.child(text(title).font(INTRO_TITLE).color(p.ink));
    }
    intro.child(text(detail).font(INTRO).color(p.muted).w_full().wrap())
}

/// *Users & Groups* switches between its two views here, with a line on what each holds.
pub(super) fn users_and_groups(p: &Palette, open: Tab) -> El {
    let mut group = row()
        .key("control-sub")
        .role("radiogroup")
        .label("Users and groups sections")
        .self_start()
        .gap(2.0)
        .padding(3.0)
        .radius(7.0)
        .fill(p.surface_2)
        .border(1.0, p.subtle);
    for (view, key, label, glyph) in [
        (Tab::Users, "control-sub-users", "Users", &glyphs::USERS),
        (Tab::Groups, "control-sub-groups", "Groups", &glyphs::SHARE),
    ] {
        let on = view == open;
        let ink = if on { p.accent_ink } else { p.muted };
        let item = row()
            .key(key)
            .role("radio")
            .label(label)
            .checked(on)
            .focusable(true)
            .h(26.0)
            .px(12.0)
            .gap(6.0)
            .center()
            .radius(5.0)
            .hover_group()
            .child(
                icon(glyph, 14.0)
                    .color(ink)
                    .group_color(if on { ink } else { p.ink })
                    .a11y_hidden(),
            )
            .child(
                text(label)
                    .font(SEGMENT)
                    .color(ink)
                    .group_color(if on { ink } else { p.ink })
                    .a11y_hidden(),
            )
            .on_press(move |s: &mut State| s.open_control_tab(view));
        group = group.child(if on {
            item.fill(p.accent).hover(p.accent_hover)
        } else {
            item.hover(p.neutral)
        });
    }
    col().w_full().gap(6.0).child(group).child(
        text(
            "Users: accounts, groups, direct overrides and effective access. \
             Groups: application rules for their members.",
        )
        .font(HINT)
        .color(p.muted)
        .w_full()
        .max_w(780.0)
        .wrap(),
    )
}

/// A section this server can't offer yet, said plainly, as classic RAST did rather than
/// show numbers it doesn't have.
pub(super) fn not_yet(p: &Palette, section: Tab) -> El {
    let (title, detail) = match section {
        Tab::Shares => (
            "Shared folders are not available yet",
            "Shared folders arrive with File Station: folders under /shared, with access set per group and account.",
        ),
        Tab::Storage => (
            "Storage is not available yet",
            "Storage locations, their capacity and each account's usage arrive with File Station.",
        ),
        Tab::Components => (
            "Components are not available yet",
            "The health of RAST's services, the host's resources and the database will show here.",
        ),
        _ => (
            "Settings are not available yet",
            "Server-wide preferences, such as the site's name, the sign-in message and the password and session policies, will be set here.",
        ),
    };
    let centred = |line: El| line.w_full().max_w(540.0).wrap().align(Align::Center);
    col()
        .key("control-not-yet")
        .w_full()
        .gap(10.0)
        .px(22.0)
        .py(46.0)
        .items_center()
        .radius(10.0)
        .fill(p.surface_2)
        .border(1.0, p.line)
        .border_dash(4.0, 3.0)
        .child(
            el().size(58.0, 58.0)
                .radius(18.0)
                .center()
                .fill(p.hover)
                .a11y_hidden()
                .child(icon(section.glyph(), 30.0).color(p.accent)),
        )
        .child(centred(text(title).font(EMPTY_TITLE).color(p.ink)))
        .child(centred(text(detail).font(EMPTY_DETAIL).color(p.muted)))
        .child(centred(
            text("This page fills in once this server offers it; nothing here is simulated.")
                .font(HINT)
                .color(p.faint),
        ))
}
