//! Control Panel → Users & Groups → Users, laid out as classic RAST's users table: a search
//! field with the count and rows per page over a table of accounts (who, role, groups,
//! status, last sign-in and an actions menu), and a pager under it.
//!
//! The actions menu and the rows-per-page list are popups drawn over the desktop
//! ([`popups`]) so the window's clip doesn't cut them, anchored to what opened them.

use rune::{Align, Color, El, Side, TextEdit, TextStyle, Tx, col, el, icon, row, text, text_input};

use super::{Page, Popup, Tab, current, messages, now_secs, sees_groups, shell, when};
use crate::State;
use crate::desktop::{AppId, Palette, glyphs, initial, menu, rgb};
use crate::server::{Account, AccountChange, Role};

/// The rows-per-page choices, as on classic RAST.
const PER_PAGE: [usize; 4] = [10, 25, 50, 100];
pub(super) const DEFAULT_PER_PAGE: usize = 25;

const CELL: TextStyle = TextStyle::new(13.0, 400.0);
const NAME: TextStyle = TextStyle::new(13.0, 560.0);
const SMALL: TextStyle = TextStyle::new(12.0, 400.0);
const HEAD: TextStyle = TextStyle::new(11.0, 650.0).tracking(0.22);
const BADGE: TextStyle = TextStyle::new(10.5, 600.0).tracking(0.1);
const CHIP: TextStyle = TextStyle::new(10.5, 400.0);
const AVATAR: TextStyle = TextStyle::new(11.0, 700.0);
const BUTTON: TextStyle = TextStyle::new(11.0, 560.0);
const ITEM: TextStyle = TextStyle::new(12.0, 450.0);
const BANNER_TITLE: TextStyle = TextStyle::new(12.5, 650.0);
const BANNER: TextStyle = TextStyle::new(12.5, 400.0).lh(18.1);

// Column widths. The User column takes what the others leave, down to its least width; below
// that the table scrolls sideways inside its frame, as classic RAST's did.
const ROLE_W: f32 = 120.0;
const GROUPS_W: f32 = 160.0;
const STATUS_W: f32 = 124.0;
const SEEN_W: f32 = 112.0;
const ACTIONS_W: f32 = 52.0;
const USER_MIN: f32 = 200.0;
const USER_MIN_COMPACT: f32 = 150.0;
/// How many group chips a row shows before "+N".
const CHIPS: usize = 2;
/// The search field's width. Classic RAST's input kept its own size (`width: auto`), about
/// this wide, rather than filling the toolbar.
const SEARCH_W: f32 = 214.0;
/// How far under the page's top the section a menu entry opens on lands.
const LANDING: f32 = 12.0;

/// A badge's colours, as classic RAST's `.badge` tones.
#[derive(Clone, Copy)]
enum Tone {
    Neutral,
    Ok,
    Info,
    Warn,
    Error,
}

/// Fill, border and text of a badge in `tone`.
fn tone(p: &Palette, tone: Tone) -> (Color, Color, Color) {
    let soft = |hex: &str, fill: f32, line: f32| (rgb(hex).alpha(fill), rgb(hex).alpha(line));
    let ((fill, line), ink) = match (tone, p.dark) {
        (Tone::Neutral, _) => return (p.surface_2, p.subtle, p.muted),
        (Tone::Info, _) => return (p.hover, p.accent_line, p.accent_text),
        (Tone::Ok, false) => ((rgb("#e8f5ee"), rgb("#bfe3cd")), p.ok_text),
        (Tone::Ok, true) => (soft("#46c07d", 0.14, 0.32), p.ok_text),
        (Tone::Warn, false) => ((rgb("#fdf3e2"), rgb("#f0d9ab")), p.warn_text),
        (Tone::Warn, true) => (soft("#e6b25c", 0.14, 0.32), p.warn_text),
        (Tone::Error, false) => ((rgb("#fcecec"), rgb("#f0c7c7")), p.danger_text),
        (Tone::Error, true) => (soft("#ef7d7d", 0.14, 0.34), p.danger_text),
    };
    (fill, line, ink)
}

/// The lines between rows (`--border-subtle`).
fn row_line(p: &Palette) -> Color {
    rgb(if p.dark { "#333c47" } else { "#e7ebef" })
}

/// Does `account` match a search? Names, role and groups, ignoring case.
fn matches(account: &Account, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || account.username.contains(&query)
        || account.display_name.to_lowercase().contains(&query)
        || account.role.label().to_lowercase().contains(&query)
        || account
            .groups
            .iter()
            .any(|group| group.to_lowercase().contains(&query))
}

fn search_model(s: &mut State) -> &mut TextEdit {
    &mut s.control.search
}

/// The accounts the search leaves, the page of them shown, and how many pages there are.
fn paged(state: &State) -> (Vec<&Account>, usize, usize) {
    let control = &state.control;
    let shown: Vec<&Account> = control
        .accounts
        .iter()
        .filter(|a| matches(a, &control.search.text))
        .collect();
    let pages = shown.len().div_ceil(control.per_page.max(1)).max(1);
    (shown, control.list_page.min(pages - 1), pages)
}

/// "1–25 of 61" over the table, or "No accounts".
fn count_line(first: usize, shown: usize, total: usize) -> String {
    if total == 0 {
        "No accounts".to_owned()
    } else {
        format!("{}–{} of {total}", first + 1, first + shown)
    }
}

/// "Page 2 of 3 · 61 items" beside the pager's buttons.
fn page_line(page: usize, pages: usize, total: usize) -> String {
    format!(
        "Page {} of {pages} · {total} item{}",
        page + 1,
        if total == 1 { "" } else { "s" }
    )
}

/// Active Super Admins: the server keeps at least one.
fn active_super_admins(accounts: &[Account]) -> usize {
    accounts
        .iter()
        .filter(|a| a.role == Role::SuperAdmin && !a.disabled)
        .count()
}

// --- actions ------------------------------------------------------------------

impl State {
    pub(crate) fn open_row_actions(&mut self, id: i64) {
        self.control.popup = Some(Popup::Actions(id));
    }

    pub(crate) fn toggle_per_page_menu(&mut self) {
        self.control.popup = match self.control.popup {
            Some(Popup::PerPage) => None,
            _ => Some(Popup::PerPage),
        };
    }

    pub(crate) fn close_users_popup(&mut self) {
        self.control.popup = None;
    }

    /// Show `count` accounts a page, from the first page.
    pub(crate) fn set_per_page(&mut self, count: usize) {
        self.control.per_page = count;
        self.control.list_page = 0;
        self.control.popup = None;
    }

    /// Show page `page` (from 0) of the accounts the search leaves.
    pub(crate) fn show_list_page(&mut self, page: usize) {
        self.control.list_page = page;
    }

    fn edit_from_list(&mut self, id: i64) {
        self.control.popup = None;
        self.open_account(id);
    }

    /// The account's page, its temporary password field focused.
    fn reset_from_list(&mut self, id: i64, tx: &mut Tx) {
        self.edit_from_list(id);
        tx.scroll_key_to("user-password-title", LANDING);
        tx.focus_when_shown("user-new-password", true);
    }

    /// The account's page, scrolled to where it is signed in.
    fn sessions_from_list(&mut self, id: i64, tx: &mut Tx) {
        self.edit_from_list(id);
        tx.scroll_key_to("user-sessions-title", LANDING);
    }

    fn unlock_from_list(&mut self, id: i64) {
        self.control.popup = None;
        self.unlock_account(id);
    }

    fn enable_from_list(&mut self, id: i64) {
        self.control.popup = None;
        let name = self.username_of(id);
        self.change_account_saying(
            id,
            AccountChange {
                disabled: Some(false),
                ..AccountChange::default()
            },
            Some(format!("Enabled {name}. They can sign in again.")),
        );
    }

    /// The first press asks to confirm in the menu; the second disables, which also signs the
    /// account out everywhere.
    fn disable_from_list(&mut self, id: i64) {
        if self.control.popup != Some(Popup::ConfirmDisable(id)) {
            self.control.popup = Some(Popup::ConfirmDisable(id));
            return;
        }
        self.control.popup = None;
        let name = self.username_of(id);
        self.change_account_saying(
            id,
            AccountChange {
                disabled: Some(true),
                ..AccountChange::default()
            },
            Some(format!("Disabled {name}. They were signed out everywhere.")),
        );
    }

    fn username_of(&self, id: i64) -> String {
        self.control
            .accounts
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.username.clone())
            .unwrap_or_default()
    }
}

// --- the list -----------------------------------------------------------------

/// The Users view: the switch to Groups, the intro, the single-Super-Admin notice, the
/// toolbar, the table and its pager. `width` is the room the page has.
pub(super) fn list(state: &State, p: &Palette, me: &Account, compact: bool, width: f32) -> El {
    let control = &state.control;
    let (shown, page, pages) = paged(state);
    let total = shown.len();
    let first = page * control.per_page.max(1);
    let rows = &shown[first.min(total)..(first + control.per_page.max(1)).min(total)];
    let count = if control.loaded {
        count_line(first, rows.len(), total)
    } else {
        String::new()
    };

    let mut page_el = col().w_full().gap(12.0);
    if sees_groups(state) {
        page_el = page_el.child(shell::users_and_groups(p, Tab::Users));
    }
    page_el = page_el.child(shell::intro(
        p,
        Some("User accounts"),
        "Create accounts, assign roles, reset passwords and end sessions. \
         A disabled account is kept but can't sign in.",
    ));
    page_el = messages(page_el, state, p);
    if control.loaded && active_super_admins(&control.accounts) == 1 {
        page_el = page_el.child(one_super_admin(p));
    }
    page_el.child(toolbar(state, p, count)).child(
        col()
            .w_full()
            .gap(4.0)
            .child(table(state, p, me, rows, compact, width))
            .child(pager(p, page, pages, total)),
    )
}

/// Classic RAST's notice that the last administrator can't be demoted or disabled.
fn one_super_admin(p: &Palette) -> El {
    let warning = rgb(if p.dark { "#e6b25c" } else { "#c07d10" });
    row()
        .key("users-one-super-admin")
        .role("status")
        .mx(10.0)
        .items_start()
        .gap(10.0)
        .px(12.0)
        .py(10.0)
        .radius(7.0)
        .fill(p.surface_2)
        .border(1.0, p.subtle)
        .border_layout()
        .child(
            icon(&glyphs::SHIELD_ALERT, 17.0)
                .color(warning)
                .mt(1.0)
                .a11y_hidden(),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(text("Only one Super Admin").font(BANNER_TITLE).color(p.ink))
                .child(
                    text(
                        "RAST will not allow the last active Super Admin to be demoted, \
                         disabled or deleted.",
                    )
                    .font(BANNER)
                    .color(p.muted)
                    .w_full()
                    .wrap(),
                ),
        )
}

/// Search on the left; the count and rows per page on the right.
fn toolbar(state: &State, p: &Palette, count: String) -> El {
    let control = &state.control;
    let search = el()
        .key("users-search-box")
        .w(SEARCH_W)
        .min_w(130.0)
        .shrink()
        .h(32.0)
        .child(
            text_input(&control.search, CELL)
                .key("users-search")
                .label("Search users")
                .single_line()
                .w_full()
                .h(32.0)
                .pl(32.0)
                .pr(10.0)
                .py(7.0)
                .radius(5.0)
                .fill(p.surface)
                .border(1.0, p.line)
                .color(p.ink)
                .placeholder("Search username or display name…", p.faint)
                // A narrow field cuts its hint at its edge, as a browser's input does.
                .clip_rounded(5.0)
                .on_edit(search_model)
                // Each new search starts on the first page, as on classic RAST.
                .on_input_change(|s: &mut State, _: &mut Tx| s.control.list_page = 0),
        )
        .child(
            icon(&glyphs::SEARCH, 16.0)
                .color(p.faint)
                .absolute(9.0, 8.0)
                .a11y_hidden(),
        );
    let open = control.popup == Some(Popup::PerPage);
    let per_page = row()
        .key("users-per-page")
        .role("button")
        .label(format!("Rows per page: {}", control.per_page))
        .expanded(open)
        .focusable(true)
        .h(32.0)
        .min_w(130.0)
        .pl(10.0)
        .pr(10.0)
        .gap(8.0)
        .center_y()
        .radius(5.0)
        .fill(p.surface)
        .border(1.0, if open { p.accent } else { p.line })
        .child(
            text(format!("{} / page", control.per_page))
                .font(CELL)
                .color(p.ink)
                .grow(1.0),
        )
        .child(icon(&glyphs::CARET_DOWN, 10.0).color(p.muted).a11y_hidden())
        .on_press(State::toggle_per_page_menu);
    row()
        .key("users-toolbar")
        .w_full()
        .gap(10.0)
        .center_y()
        .flex_wrap()
        .child(search)
        .child(el().grow(1.0))
        .child(text(count).key("users-count").font(SMALL).color(p.muted))
        .child(per_page)
}

/// The accounts on this page in classic RAST's table, framed, scrolling sideways when the
/// columns don't fit `width`. A phone (`compact`) leaves out Groups and Last sign-in.
fn table(
    state: &State,
    p: &Palette,
    me: &Account,
    rows: &[&Account],
    compact: bool,
    width: f32,
) -> El {
    let control = &state.control;
    let user_min = if compact { USER_MIN_COMPACT } else { USER_MIN };
    let fixed = ROLE_W + STATUS_W + ACTIONS_W + if compact { 0.0 } else { GROUPS_W + SEEN_W };
    // Inside the frame's 1 pt border.
    let table_w = (width - 2.0).max(user_min + fixed);
    let mut body = col()
        .key("users-rows")
        .w(table_w)
        .child(head(p, compact, user_min));
    if !control.loaded {
        for i in 0..5 {
            body = body.child(skeleton(p, compact, user_min, i == 4));
        }
    } else if rows.is_empty() {
        let query = control.search.text.trim();
        body = body.child(
            el().key("users-none")
                .w_full()
                .px(16.0)
                .py(32.0)
                .center()
                .child(
                    text(if query.is_empty() {
                        "No user accounts exist yet.".to_owned()
                    } else {
                        format!("No accounts match “{query}”.")
                    })
                    .font(SMALL)
                    .color(p.muted),
                ),
        );
    } else {
        for (i, account) in rows.iter().enumerate() {
            let last = i + 1 == rows.len();
            body = body.child(account_row(state, p, me, account, compact, user_min, last));
        }
    }
    col()
        .key("users-table")
        .w_full()
        .radius(7.0)
        .fill(p.surface)
        .border(1.0, p.subtle)
        .border_layout()
        .child(
            col().w_full().clip_rounded(6.0).child(
                col()
                    .key("users-table-scroll")
                    .w_full()
                    .scroll_x()
                    .child(body),
            ),
        )
}

/// A table cell: 8 pt above and below, 12 at the sides.
fn cell() -> El {
    row().px(12.0).py(8.0).center_y().min_w(0.0)
}

fn head(p: &Palette, compact: bool, user_min: f32) -> El {
    let th = |label: &'static str| {
        row()
            .h(32.0)
            .px(12.0)
            .center_y()
            .child(text(label).font(HEAD).color(p.muted))
    };
    let mut head = row()
        .key("users-head")
        .w_full()
        .fill(p.surface_2)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.subtle)
        .child(th("User").grow(1.0).min_w(user_min))
        .child(th("Role").key("users-col-role").w(ROLE_W));
    if !compact {
        head = head.child(th("Groups").key("users-col-groups").w(GROUPS_W));
    }
    head = head.child(th("Status").w(STATUS_W));
    if !compact {
        head = head.child(th("Last sign-in").key("users-col-seen").w(SEEN_W));
    }
    head.child(el().w(ACTIONS_W).label("Actions"))
}

/// A row while the accounts load: a grey line per cell.
fn skeleton(p: &Palette, compact: bool, user_min: f32, last: bool) -> El {
    let bar = |w: f32| {
        row()
            .px(12.0)
            .py(13.0)
            .center_y()
            .child(el().w(w).h(10.0).radius(5.0).fill(p.surface_3))
    };
    let mut line = row()
        .w_full()
        .a11y_hidden()
        .child(bar(user_min * 0.6).grow(1.0).min_w(user_min))
        .child(bar(ROLE_W - 24.0).w(ROLE_W));
    if !compact {
        line = line.child(bar(GROUPS_W - 24.0).w(GROUPS_W));
    }
    line = line.child(bar(STATUS_W - 24.0).w(STATUS_W));
    if !compact {
        line = line.child(bar(SEEN_W - 24.0).w(SEEN_W));
    }
    line = line.child(el().w(ACTIONS_W));
    if last {
        line
    } else {
        line.border_sides([0.0, 0.0, 1.0, 0.0], row_line(p))
    }
}

/// One account. A double-click opens it, as on classic RAST; its menu has the rest.
fn account_row(
    state: &State,
    p: &Palette,
    me: &Account,
    account: &Account,
    compact: bool,
    user_min: f32,
    last: bool,
) -> El {
    let id = account.id;
    let mut line = row()
        .key(format!("user-{id}"))
        .label(format!("{}, {}", account.name(), account.role.label()))
        .focusable(false)
        .w_full()
        .center_y()
        .hover(p.neutral)
        .on_double_press(move |s: &mut State| s.open_account(id))
        .child(
            cell()
                .grow(1.0)
                .min_w(user_min)
                .child(who(p, account, account.id == me.id)),
        )
        .child(cell().w(ROLE_W).child(role_badge(p, account.role)));
    if !compact {
        line = line.child(cell().w(GROUPS_W).child(groups(p, &account.groups)));
    }
    line = line.child(cell().w(STATUS_W).child(status(p, account)));
    if !compact {
        line = line.child(cell().w(SEEN_W).child(seen(p, account.last_login_at)));
    }
    let open = matches!(
        state.control.popup,
        Some(Popup::Actions(open) | Popup::ConfirmDisable(open)) if open == id
    );
    line = line.child(
        cell()
            .w(ACTIONS_W)
            .justify_end()
            .child(actions_button(p, account, open)),
    );
    if last {
        line
    } else {
        line.border_sides([0.0, 0.0, 1.0, 0.0], row_line(p))
    }
}

/// The avatar, the name (with *You* on your own row) and the username.
/// The avatar, name and username. A name too long for the column ends in "…" and keeps its
/// "YOU", as classic RAST's `.user-cell-name` clipped it.
fn who(p: &Palette, account: &Account, yourself: bool) -> El {
    let mut name = row().w_full().gap(6.0).center_y().min_w(0.0).child(
        text(account.name().to_owned())
            .font(NAME)
            .color(p.ink)
            .truncate()
            .min_w(0.0)
            .shrink(),
    );
    if yourself {
        name = name.child(badge(p, "YOU", Tone::Neutral).key(format!("user-{}-you", account.id)));
    }
    row()
        .gap(9.0)
        .center_y()
        .min_w(0.0)
        .shrink()
        .child(
            el().size(26.0, 26.0)
                .radius(8.0)
                .fill(p.accent)
                .center()
                .a11y_hidden()
                .child(
                    text(initial(account.name()))
                        .font(AVATAR)
                        .color(p.accent_ink),
                ),
        )
        .child(
            col().grow(1.0).min_w(0.0).shrink().child(name).child(
                text(format!("@{}", account.username))
                    .font(SMALL)
                    .color(p.faint)
                    .w_full()
                    .truncate(),
            ),
        )
}

fn badge(p: &Palette, label: impl Into<String>, t: Tone) -> El {
    let (fill, line, ink) = tone(p, t);
    row()
        .h(20.0)
        .px(7.0)
        .center()
        .radius(10.0)
        .fill(fill)
        .border(1.0, line)
        .child(text(label).font(BADGE).color(ink))
}

/// Super Admin in the warning tone and Admin in the accent's, as classic RAST marked its
/// super administrators and administrators.
fn role_badge(p: &Palette, role: Role) -> El {
    let t = match role {
        Role::SuperAdmin => Tone::Warn,
        Role::Admin => Tone::Info,
        Role::User => Tone::Neutral,
    };
    badge(p, role.label(), t)
}

/// The first two groups as chips and "+N" for the rest, or a dash.
fn groups(p: &Palette, names: &[String]) -> El {
    if names.is_empty() {
        return text("—").font(SMALL).color(p.faint);
    }
    let chip = |label: String, ink: Color| {
        row()
            .h(20.0)
            .px(7.0)
            .center_y()
            .max_w(GROUPS_W - 24.0)
            .radius(10.0)
            .fill(p.surface_2)
            .border(1.0, p.subtle)
            .child(text(label).font(CHIP).color(ink).truncate().min_w(0.0))
    };
    let mut chips = row().w_full().flex_wrap().gap(4.0).center_y();
    for name in names.iter().take(CHIPS) {
        chips = chips.child(chip(name.clone(), p.muted));
    }
    if names.len() > CHIPS {
        chips = chips.child(chip(format!("+{}", names.len() - CHIPS), p.faint));
    }
    chips
}

/// One badge: Disabled, Locked, a new password due, or Active.
fn status(p: &Palette, account: &Account) -> El {
    if account.disabled {
        badge(p, "Disabled", Tone::Neutral)
    } else if account.locked() {
        badge(p, "Locked", Tone::Error).help("Too many wrong passwords. Unlock it from its menu.")
    } else if account.must_change_password {
        badge(p, "Password due", Tone::Warn)
            .help("They choose a new password at their next sign-in.")
    } else {
        badge(p, "Active", Tone::Ok)
    }
}

/// When the account last signed in, as classic RAST said it, the exact time as a tooltip.
fn seen(p: &Palette, at: Option<i64>) -> El {
    match at.filter(|at| *at > 0) {
        None => text("Never").font(SMALL).color(p.muted),
        Some(at) => text(relative(at, now_secs()))
            .font(SMALL)
            .color(p.ink)
            .help(when(Some(at))),
    }
}

/// "just now", "5 min ago", "3 h ago", "12 d ago", else the date (2026-11-21).
fn relative(at: i64, now: i64) -> String {
    let gone = now - at;
    if gone < 45 {
        return "just now".to_owned();
    }
    let minutes = (gone / 60).max(1);
    if minutes < 60 {
        return format!("{minutes} min ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} h ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days} d ago");
    }
    let date = rune::time::LocalTime::at(at * 1000);
    format!("{}-{:02}-{:02}", date.year, date.month, date.day)
}

/// The row's ⋮ button, which opens its menu.
fn actions_button(p: &Palette, account: &Account, open: bool) -> El {
    let id = account.id;
    row()
        .key(format!("user-{id}-actions"))
        .role("button")
        .label(format!("Actions for {}", account.username))
        .help("Account actions")
        .expanded(open)
        .focusable(true)
        .size(27.0, 27.0)
        .center()
        .radius(5.0)
        .fill(if open { p.neutral } else { Color::TRANSPARENT })
        .hover(p.neutral)
        .hover_group()
        .child(
            icon(&glyphs::MORE, 16.0)
                .color(p.muted)
                .group_color(p.ink)
                .a11y_hidden(),
        )
        .on_press(move |s: &mut State| s.open_row_actions(id))
}

/// "Page 2 of 3 · 61 items" with Previous and Next, under the table's right end.
fn pager(p: &Palette, page: usize, pages: usize, total: usize) -> El {
    let label = page_line(page, pages, total);
    row()
        .key("users-pager")
        .w_full()
        .gap(8.0)
        .center_y()
        .flex_wrap()
        .child(el().grow(1.0))
        .child(
            text(label)
                .key("users-page-label")
                .font(SMALL)
                .color(p.muted),
        )
        .child(small_button(
            p,
            "users-prev",
            "Previous",
            page > 0,
            move |s: &mut State| s.show_list_page(page.saturating_sub(1)),
        ))
        .child(small_button(
            p,
            "users-next",
            "Next",
            page + 1 < pages,
            move |s: &mut State| s.show_list_page(page + 1),
        ))
}

/// Classic RAST's `.btn.btn-sm`; a disabled one is faint with a lighter border.
fn small_button(
    p: &Palette,
    key: &'static str,
    label: &'static str,
    enabled: bool,
    press: impl Fn(&mut State) + 'static,
) -> El {
    let button = row()
        .key(key)
        .role("button")
        .label(label)
        .h(27.0)
        .px(9.0)
        .center()
        .radius(5.0);
    if enabled {
        button
            .focusable(true)
            .fill(p.surface)
            .border(1.0, p.line)
            .hover(p.neutral)
            .child(text(label).font(BUTTON).color(p.ink))
            .on_press(press)
    } else {
        button
            .disabled(true)
            .border(1.0, row_line(p))
            .child(text(label).font(BUTTON).color(p.faint))
    }
}

// --- popups -------------------------------------------------------------------

/// The open popup of the Users view, if any, for the desktop to draw over everything: a
/// scrim that closes it and the menu, anchored to its button.
pub(super) fn popups(state: &State, p: &Palette) -> Vec<El> {
    let control = &state.control;
    let (Some(popup), Some(me)) = (control.popup, state.session.as_ref()) else {
        return Vec::new();
    };
    if current(state) != Tab::Users
        || control.page != Page::List
        || state.desktop.windows.focused() != Some(AppId::Control)
    {
        return Vec::new();
    }
    let sheet = match popup {
        Popup::PerPage => per_page_menu(p, control.per_page),
        Popup::Actions(id) | Popup::ConfirmDisable(id) => {
            let Some(account) = control.accounts.iter().find(|a| a.id == id) else {
                return Vec::new();
            };
            actions_menu(p, me, account, popup == Popup::ConfirmDisable(id))
        }
    };
    vec![
        el().key("users-popup-scrim")
            .role("none")
            .focusable(false)
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .on_press(State::close_users_popup),
        sheet.modal(true, State::close_users_popup),
    ]
}

/// Edit, reset the password, see the sessions; then unlock, enable or disable. What this
/// account may not do to that one is dimmed.
fn actions_menu(p: &Palette, me: &Account, account: &Account, confirm: bool) -> El {
    let id = account.id;
    let manage = me.id != id && me.role.may_manage(account.role);
    let mut sheet = menu::sheet(p, "user-menu", &format!("Actions for {}", account.username))
        .anchored(format!("user-{id}-actions"), Side::Bottom, Align::End, 4.0)
        .anchored_round()
        .child(
            menu::item(p, "user-menu-edit", &glyphs::EDIT, "Edit user", p.ink)
                .on_press(move |s: &mut State| s.edit_from_list(id)),
        );
    sheet = sheet.child(if manage {
        menu::item(
            p,
            "user-menu-password",
            &glyphs::KEY,
            "Reset password",
            p.ink,
        )
        .on_press_tx(move |s: &mut State, tx: &mut Tx| s.reset_from_list(id, tx))
    } else {
        menu::dimmed(p, "user-menu-password", &glyphs::KEY, "Reset password")
    });
    sheet = sheet
        .child(if manage {
            menu::item(
                p,
                "user-menu-sessions",
                &glyphs::SESSIONS,
                "View sessions",
                p.ink,
            )
            .on_press_tx(move |s: &mut State, tx: &mut Tx| s.sessions_from_list(id, tx))
        } else {
            menu::dimmed(p, "user-menu-sessions", &glyphs::SESSIONS, "View sessions")
        })
        .child(menu::separator(row_line(p)));
    if account.locked() {
        sheet = sheet.child(if manage {
            menu::item(
                p,
                "user-menu-unlock",
                &glyphs::UNLOCK,
                "Unlock account",
                p.ink,
            )
            .on_press(move |s: &mut State| s.unlock_from_list(id))
        } else {
            menu::dimmed(p, "user-menu-unlock", &glyphs::UNLOCK, "Unlock account")
        });
    }
    sheet.child(match (account.disabled, manage) {
        (true, true) => menu::item(
            p,
            "user-menu-enable",
            &glyphs::CHECK,
            "Enable account",
            p.ink,
        )
        .on_press(move |s: &mut State| s.enable_from_list(id)),
        (true, false) => menu::dimmed(p, "user-menu-enable", &glyphs::CHECK, "Enable account"),
        (false, true) => {
            let label = if confirm {
                format!("Press again to disable {}", account.username)
            } else {
                "Disable account".to_owned()
            };
            menu::item(p, "user-menu-disable", &glyphs::POWER, label, p.danger_text)
                .hover(tone(p, Tone::Error).0)
                .on_press(move |s: &mut State| s.disable_from_list(id))
        }
        (false, false) => menu::dimmed(p, "user-menu-disable", &glyphs::POWER, "Disable account"),
    })
}

/// 10, 25, 50 or 100 accounts a page, the current one checked.
fn per_page_menu(p: &Palette, current: usize) -> El {
    let mut sheet = menu::sheet(p, "users-per-page-menu", "Rows per page")
        .min_w(130.0)
        .anchored("users-per-page", Side::Bottom, Align::End, 4.0)
        .anchored_round();
    for count in PER_PAGE {
        let on = count == current;
        let label = format!("{count} / page");
        let mark = el().size(16.0, 16.0).center().a11y_hidden();
        sheet = sheet.child(
            row()
                .key(format!("users-per-page-{count}"))
                .role("menuitemradio")
                .label(label.clone())
                .checked(on)
                .focusable(true)
                .w_full()
                .h(30.0)
                .px(8.0)
                .gap(9.0)
                .center_y()
                .radius(5.0)
                .hover(p.neutral)
                .child(if on {
                    mark.child(icon(&glyphs::CHECK, 14.0).color(p.accent_text))
                } else {
                    mark
                })
                .child(text(label).font(ITEM).color(p.ink))
                .on_press(move |s: &mut State| s.set_per_page(count)),
        );
    }
    sheet
}

#[cfg(test)]
mod tests {
    use super::{count_line, page_line, relative};

    #[test]
    fn the_count_and_the_pager_read_as_on_classic_rast() {
        assert_eq!(count_line(0, 7, 7), "1–7 of 7");
        assert_eq!(count_line(25, 25, 61), "26–50 of 61");
        assert_eq!(count_line(0, 0, 0), "No accounts");
        assert_eq!(page_line(0, 1, 7), "Page 1 of 1 · 7 items");
        assert_eq!(page_line(1, 3, 61), "Page 2 of 3 · 61 items");
        assert_eq!(page_line(0, 1, 1), "Page 1 of 1 · 1 item");
    }

    #[test]
    fn last_sign_in_reads_as_classic_rast_said_it() {
        let now = 1_795_000_000;
        assert_eq!(relative(now - 10, now), "just now");
        assert_eq!(relative(now - 50, now), "1 min ago");
        assert_eq!(relative(now - 59 * 60, now), "59 min ago");
        assert_eq!(relative(now - 5 * 3_600, now), "5 h ago");
        assert_eq!(relative(now - 12 * 86_400, now), "12 d ago");
        assert!(relative(now - 400 * 86_400, now).starts_with("2025-"));
    }
}
