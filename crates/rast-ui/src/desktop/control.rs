//! Control Panel: Users (for administrators), Profile, and, natively, the
//! connection settings. Lists stay calm (an icon, a name, one line, one
//! action); everything about one account is on its own page.

use rune::{Color, El, ScrollSpec, TextEdit, TextStyle, Tx, col, el, row, text, text_input};

use super::{Palette, icon, initial};
use crate::State;
use crate::server::{Account, AccountChange, Failure, NewAccount, Reply, Role};

const TITLE: TextStyle = TextStyle::new(15.0, 600.0);
const BODY: TextStyle = TextStyle::new(13.0, 400.0);
const NAME: TextStyle = TextStyle::new(13.0, 500.0);
const DETAIL: TextStyle = TextStyle::new(11.5, 400.0);
const LABEL: TextStyle = TextStyle::new(12.0, 500.0);
const SECTION: TextStyle = TextStyle::new(10.5, 500.0).tracking(0.7);
const BUTTON: TextStyle = TextStyle::new(12.5, 500.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Users,
    Profile,
    Connection,
}

impl Tab {
    fn key(self) -> &'static str {
        match self {
            Self::Users => "control-tab-users",
            Self::Profile => "control-tab-profile",
            Self::Connection => "control-tab-connection",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Users => "Users",
            Self::Profile => "Profile",
            Self::Connection => "Connection",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::Profile => "profile",
            Self::Connection => "network",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    List,
    Create,
    Account(i64),
}

pub(crate) struct ControlPanel {
    pub(crate) tab: Tab,
    pub(crate) page: Page,
    pub(crate) accounts: Vec<Account>,
    pub(crate) loaded: bool,
    pub(crate) busy: bool,
    pub(crate) error: Option<String>,
    pub(crate) notice: Option<String>,
    pub(crate) new_username: TextEdit,
    pub(crate) new_display: TextEdit,
    pub(crate) new_password: TextEdit,
    pub(crate) new_role: Role,
    pub(crate) reset_password: TextEdit,
    pub(crate) confirm_delete: bool,
    pub(crate) current_password: TextEdit,
    pub(crate) next_password: TextEdit,
    pub(crate) confirm_password: TextEdit,
}

impl Default for ControlPanel {
    fn default() -> Self {
        Self {
            tab: Tab::Connection,
            page: Page::List,
            accounts: Vec::new(),
            loaded: false,
            busy: false,
            error: None,
            notice: None,
            new_username: TextEdit::new(""),
            new_display: TextEdit::new(""),
            new_password: TextEdit::new("").secure(),
            new_role: Role::User,
            reset_password: TextEdit::new("").secure(),
            confirm_delete: false,
            current_password: TextEdit::new("").secure(),
            next_password: TextEdit::new("").secure(),
            confirm_password: TextEdit::new("").secure(),
        }
    }
}

fn tabs(state: &State) -> Vec<Tab> {
    match &state.session {
        Some(session) if session.role.is_admin() => vec![Tab::Users, Tab::Profile],
        Some(_) => vec![Tab::Profile],
        None => vec![Tab::Connection],
    }
}

// --- actions ------------------------------------------------------------------

impl State {
    /// Opening Control Panel lands on Users for administrators, else Profile.
    pub(crate) fn reset_control(&mut self) {
        let tab = tabs(self)[0];
        self.control = ControlPanel::default();
        self.open_control_tab(tab);
    }

    pub(crate) fn open_control_tab(&mut self, tab: Tab) {
        self.control.tab = tab;
        self.control.page = Page::List;
        self.control.error = None;
        self.control.notice = None;
        if tab == Tab::Users && !self.control.loaded && !self.control.busy {
            self.control.busy = true;
            self.calls().accounts();
        }
    }

    pub(crate) fn open_account(&mut self, id: i64) {
        let control = &mut self.control;
        control.page = Page::Account(id);
        control.reset_password = TextEdit::new("").secure();
        control.confirm_delete = false;
        control.error = None;
        control.notice = None;
    }

    pub(crate) fn back_to_accounts(&mut self) {
        self.control.page = Page::List;
        self.control.error = None;
        self.control.confirm_delete = false;
    }

    pub(crate) fn start_create_account(&mut self) {
        let control = &mut self.control;
        control.page = Page::Create;
        control.new_username = TextEdit::new("");
        control.new_display = TextEdit::new("");
        control.new_password = TextEdit::new("").secure();
        control.new_role = Role::User;
        control.error = None;
        control.notice = None;
    }

    pub(crate) fn submit_create_account(&mut self) {
        if self.control.busy {
            return;
        }
        let username = self.control.new_username.text.trim().to_lowercase();
        if username.is_empty() {
            self.control.error = Some("Enter a username.".to_owned());
            return;
        }
        if self.control.new_password.text.chars().count() < 8 {
            self.control.error = Some("Use at least 8 characters for the password.".to_owned());
            return;
        }
        let account = NewAccount {
            username,
            display_name: self.control.new_display.text.trim().to_owned(),
            password: self.control.new_password.text.clone(),
            role: self.control.new_role,
        };
        self.control.busy = true;
        self.control.error = None;
        self.calls().create(&account);
    }

    fn change_account(&mut self, id: i64, change: AccountChange) {
        if self.control.busy {
            return;
        }
        self.control.busy = true;
        self.control.error = None;
        self.control.notice = None;
        self.calls().update(id, &change);
    }

    pub(crate) fn set_account_role(&mut self, id: i64, role: Role) {
        self.change_account(
            id,
            AccountChange {
                role: Some(role),
                ..AccountChange::default()
            },
        );
    }

    pub(crate) fn set_account_disabled(&mut self, id: i64, disabled: bool) {
        self.change_account(
            id,
            AccountChange {
                disabled: Some(disabled),
                ..AccountChange::default()
            },
        );
    }

    pub(crate) fn reset_account_password(&mut self, id: i64) {
        let password = self.control.reset_password.text.clone();
        if password.chars().count() < 8 {
            self.control.error = Some("Use at least 8 characters for the new password.".to_owned());
            return;
        }
        self.change_account(
            id,
            AccountChange {
                password: Some(password),
                ..AccountChange::default()
            },
        );
    }

    /// The first press asks to confirm; the second deletes.
    pub(crate) fn delete_account(&mut self, id: i64) {
        if self.control.busy {
            return;
        }
        if !self.control.confirm_delete {
            self.control.confirm_delete = true;
            return;
        }
        self.control.busy = true;
        self.control.error = None;
        self.calls().delete(id);
    }

    pub(crate) fn change_own_password(&mut self) {
        if self.control.busy {
            return;
        }
        let control = &mut self.control;
        control.notice = None;
        if control.next_password.text.chars().count() < 8 {
            control.error = Some("Use at least 8 characters for the new password.".to_owned());
            return;
        }
        if control.next_password.text != control.confirm_password.text {
            control.error = Some("The new passwords do not match.".to_owned());
            return;
        }
        control.busy = true;
        control.error = None;
        let (current, next) = (
            control.current_password.text.clone(),
            control.next_password.text.clone(),
        );
        self.calls().change_password(&current, &next);
    }

    /// Apply a finished server call.
    pub(crate) fn apply_reply(&mut self, reply: Reply, tx: &mut Tx) {
        self.control.busy = false;
        let failure = match reply {
            Reply::Accounts(Ok(accounts)) => {
                self.control.accounts = accounts;
                self.control.loaded = true;
                None
            }
            Reply::Created(Ok(account)) => {
                self.control.notice = Some(format!("Created {}.", account.username));
                self.control.accounts.push(account);
                sort(&mut self.control.accounts);
                self.control.page = Page::List;
                None
            }
            Reply::Updated(Ok(account)) => {
                self.control.reset_password = TextEdit::new("").secure();
                self.control.notice = Some("Saved.".to_owned());
                if let Some(slot) = self
                    .control
                    .accounts
                    .iter_mut()
                    .find(|a| a.id == account.id)
                {
                    *slot = account;
                }
                sort(&mut self.control.accounts);
                None
            }
            Reply::Deleted(id, Ok(())) => {
                if let Some(gone) = self.control.accounts.iter().find(|a| a.id == id) {
                    self.control.notice = Some(format!("Deleted {}.", gone.username));
                }
                self.control.accounts.retain(|a| a.id != id);
                self.control.page = Page::List;
                self.control.confirm_delete = false;
                None
            }
            Reply::PasswordChanged(Ok(())) => {
                self.control.current_password = TextEdit::new("").secure();
                self.control.next_password = TextEdit::new("").secure();
                self.control.confirm_password = TextEdit::new("").secure();
                self.control.notice = Some(
                    "Password changed. Other sessions of your account were signed out.".to_owned(),
                );
                None
            }
            Reply::Accounts(Err(failure))
            | Reply::Created(Err(failure))
            | Reply::Updated(Err(failure))
            | Reply::Deleted(_, Err(failure))
            | Reply::PasswordChanged(Err(failure)) => Some(failure),
        };
        if let Some(failure) = failure {
            self.failed(failure, tx);
        }
    }

    fn failed(&mut self, failure: Failure, tx: &mut Tx) {
        if failure.signed_out() && self.session.is_some() {
            #[cfg(target_arch = "wasm32")]
            crate::browser::sign_out_with(self, tx, "Your session ended. Sign in again.");
            #[cfg(not(target_arch = "wasm32"))]
            {
                let _ = tx;
                self.sign_out();
            }
            return;
        }
        self.control.confirm_delete = false;
        self.control.error = Some(failure.message);
    }
}

fn sort(accounts: &mut [Account]) {
    let rank = |role: Role| Role::ALL.iter().position(|r| *r == role).unwrap_or(3);
    accounts.sort_by(|a, b| {
        (rank(a.role), a.username.as_str()).cmp(&(rank(b.role), b.username.as_str()))
    });
}

// --- views --------------------------------------------------------------------

pub(super) fn view(state: &State, p: &Palette, compact: bool) -> El {
    let tabs = tabs(state);
    let tab = if tabs.contains(&state.control.tab) {
        state.control.tab
    } else {
        tabs[0]
    };
    let content = match (tab, &state.session) {
        (Tab::Users, Some(me)) => users_page(state, p, me),
        (Tab::Profile, Some(me)) => profile_page(state, p, me),
        _ => return scroller().child(crate::ui::settings_body(state)),
    };
    let content = scroller().child(
        col()
            .w_full()
            .padding(if compact { 14.0 } else { 18.0 })
            .gap(12.0)
            .child(content),
    );
    let nav_item = |item: Tab| {
        let selected = item == tab;
        let mut entry = row()
            .key(item.key())
            .role("tab")
            .label(item.label())
            .checked(selected)
            .h(34.0)
            .px(10.0)
            .gap(8.0)
            .center_y()
            .radius(7.0)
            .hover(p.hover)
            .child(icon(state, item.icon(), 16.0).a11y_hidden())
            .child(text(item.label()).font(LABEL).color(p.ink))
            .on_press(move |s: &mut State| s.open_control_tab(item));
        if selected {
            entry = entry.fill(p.hover);
        }
        entry
    };
    if compact {
        let mut strip = row().w_full().gap(4.0).px(10.0).pt(10.0);
        for item in tabs {
            strip = strip.child(nav_item(item));
        }
        return col()
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(strip)
            .child(content);
    }
    let mut nav = col()
        .key("control-nav")
        .w(168.0)
        .padding(10.0)
        .gap(2.0)
        .border_sides([0.0, 1.0, 0.0, 0.0], p.line);
    for item in tabs {
        nav = nav.child(nav_item(item));
    }
    row()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(nav)
        .child(content)
}

/// The part of the window that scrolls; the sidebar stays put.
fn scroller() -> El {
    col()
        .key("control-content")
        .w_full()
        .grow(1.0)
        .min_w(0.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
}

fn users_page(state: &State, p: &Palette, me: &Account) -> El {
    let control = &state.control;
    match control.page {
        Page::Create => create_page(state, p, me),
        Page::Account(id) => match control.accounts.iter().find(|a| a.id == id) {
            Some(account) => account_page(state, p, me, account),
            None => users_list(state, p, me),
        },
        Page::List => users_list(state, p, me),
    }
}

fn users_list(state: &State, p: &Palette, me: &Account) -> El {
    let control = &state.control;
    let count = if control.loaded {
        match control.accounts.len() {
            1 => "1 account".to_owned(),
            n => format!("{n} accounts"),
        }
    } else {
        "Loading accounts…".to_owned()
    };
    let mut page = col().w_full().gap(12.0).child(
        row()
            .w_full()
            .gap(10.0)
            .center_y()
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(2.0)
                    .child(text("Users").font(TITLE).color(p.ink))
                    .child(text(count).font(DETAIL).color(p.muted)),
            )
            .child(
                button(p, "users-create", "Create user", Look::Primary)
                    .on_press(State::start_create_account),
            ),
    );
    page = messages(page, state, p);
    let mut list = col().key("users-list").w_full().gap(2.0);
    for account in &control.accounts {
        let id = account.id;
        let mut line = format!("@{} · {}", account.username, account.role.label());
        if account.id == me.id {
            line.push_str(" · You");
        }
        let mut entry = row()
            .key(format!("user-{id}"))
            .role("button")
            .label(format!("{}, {}", account.name(), account.role.label()))
            .w_full()
            .h(50.0)
            .px(10.0)
            .gap(10.0)
            .center_y()
            .radius(8.0)
            .hover(p.hover)
            .child(avatar(p, account, 32.0))
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(2.0)
                    .child(
                        text(account.name().to_owned())
                            .font(NAME)
                            .color(p.ink)
                            .truncate(),
                    )
                    .child(text(line).font(DETAIL).color(p.muted).truncate()),
            );
        if account.disabled {
            entry = entry.child(badge(p, "Disabled"));
        }
        list = list.child(
            entry
                .child(text("›").font(BODY).color(p.muted).a11y_hidden())
                .on_press(move |s: &mut State| s.open_account(id)),
        );
    }
    page.child(list)
}

fn account_page(state: &State, p: &Palette, me: &Account, account: &Account) -> El {
    let control = &state.control;
    let id = account.id;
    let yourself = account.id == me.id;
    let manage = !yourself && me.role.may_manage(account.role);
    let mut page = col().w_full().gap(12.0).child(back(p)).child(
        row()
            .w_full()
            .gap(12.0)
            .center_y()
            .child(avatar(p, account, 44.0))
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(2.0)
                    .child(
                        text(account.name().to_owned())
                            .font(TITLE)
                            .color(p.ink)
                            .truncate(),
                    )
                    .child(
                        text(format!("@{} · {}", account.username, account.role.label()))
                            .font(DETAIL)
                            .color(p.muted),
                    ),
            ),
    );
    page = messages(page, state, p);

    page = page.child(section(p, "ROLE"));
    let roles: Vec<Role> = Role::ALL
        .into_iter()
        .filter(|role| me.role.may_manage(*role))
        .collect();
    if manage && roles.len() > 1 {
        page = page
            .child(segmented(
                p,
                "user-role",
                &roles,
                account.role,
                move |role| move |s: &mut State| s.set_account_role(id, role),
            ))
            .child(text(account.role.summary()).font(DETAIL).color(p.muted));
    } else {
        page = page
            .child(text(account.role.label()).font(BODY).color(p.ink))
            .child(
                text(if yourself {
                    "You can't change your own role."
                } else {
                    "Only a Super Admin can change roles or manage administrators."
                })
                .font(DETAIL)
                .color(p.muted)
                .w_full()
                .wrap(),
            );
    }

    if manage {
        let enabled = !account.disabled;
        page = page.child(section(p, "SIGN-IN")).child(
            row()
                .w_full()
                .gap(10.0)
                .center_y()
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(2.0)
                        .child(
                            text(if enabled { "Can sign in" } else { "Disabled" })
                                .font(BODY)
                                .color(p.ink),
                        )
                        .child(
                            text("Disabling signs the account out everywhere.")
                                .font(DETAIL)
                                .color(p.muted),
                        ),
                )
                .child(
                    switch(p, "user-enabled", "Can sign in", enabled)
                        .on_press(move |s: &mut State| s.set_account_disabled(id, enabled)),
                ),
        );
        page = page.child(section(p, "PASSWORD")).child(
            row()
                .w_full()
                .gap(8.0)
                .child(
                    field(
                        p,
                        &control.reset_password,
                        "user-new-password",
                        "New password",
                        "at least 8 characters",
                        |s| &mut s.control.reset_password,
                    )
                    .grow(1.0)
                    .min_w(0.0),
                )
                .child(
                    col().child(el().h(18.0)).child(
                        button(p, "user-set-password", "Set password", Look::Plain)
                            .on_press(move |s: &mut State| s.reset_account_password(id)),
                    ),
                ),
        );
    }

    page = page
        .child(section(p, "DETAILS"))
        .child(fact(p, "Created", &when(Some(account.created_at))))
        .child(fact(p, "Last sign-in", &when(account.last_login_at)));

    if manage {
        let label = if control.confirm_delete {
            format!("Press again to delete {}", account.username)
        } else {
            "Delete account".to_owned()
        };
        page = page.child(divider(p)).child(
            row().child(
                button(p, "user-delete", &label, Look::Danger)
                    .on_press(move |s: &mut State| s.delete_account(id)),
            ),
        );
    }
    page
}

fn create_page(state: &State, p: &Palette, me: &Account) -> El {
    let control = &state.control;
    let roles: Vec<Role> = Role::ALL
        .into_iter()
        .filter(|role| me.role.may_manage(*role))
        .collect();
    let mut page = col()
        .key("create-user")
        .w_full()
        .max_w(440.0)
        .gap(12.0)
        .child(back(p))
        .child(text("Create user").font(TITLE).color(p.ink))
        .child(field(
            p,
            &control.new_username,
            "new-username",
            "Username",
            "e.g. alice",
            |s| &mut s.control.new_username,
        ))
        .child(field(
            p,
            &control.new_display,
            "new-display",
            "Display name (optional)",
            "e.g. Alice Smith",
            |s| &mut s.control.new_display,
        ))
        .child(field(
            p,
            &control.new_password,
            "new-password",
            "Password",
            "at least 8 characters",
            |s| &mut s.control.new_password,
        ))
        .child(section(p, "ROLE"));
    page = if roles.len() > 1 {
        page.child(segmented(p, "new-role", &roles, control.new_role, |role| {
            move |s: &mut State| s.control.new_role = role
        }))
        .child(text(control.new_role.summary()).font(DETAIL).color(p.muted))
    } else {
        page.child(text("User").font(BODY).color(p.ink)).child(
            text("Admins create users. Only a Super Admin creates administrators.")
                .font(DETAIL)
                .color(p.muted)
                .w_full()
                .wrap(),
        )
    };
    page = messages(page, state, p);
    page.child(
        row()
            .gap(8.0)
            .pt(4.0)
            .child(
                button(
                    p,
                    "create-user-submit",
                    if control.busy {
                        "Creating…"
                    } else {
                        "Create user"
                    },
                    Look::Primary,
                )
                .on_press(State::submit_create_account),
            )
            .child(
                button(p, "create-user-cancel", "Cancel", Look::Plain)
                    .on_press(State::back_to_accounts),
            ),
    )
}

fn profile_page(state: &State, p: &Palette, me: &Account) -> El {
    let control = &state.control;
    let mut page = col()
        .key("profile")
        .w_full()
        .max_w(440.0)
        .gap(12.0)
        .child(
            row()
                .w_full()
                .gap(12.0)
                .center_y()
                .child(avatar(p, me, 44.0))
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(2.0)
                        .child(
                            text(me.name().to_owned())
                                .font(TITLE)
                                .color(p.ink)
                                .truncate(),
                        )
                        .child(
                            text(format!("@{} · {}", me.username, me.role.label()))
                                .font(DETAIL)
                                .color(p.muted),
                        ),
                ),
        )
        .child(section(p, "CHANGE PASSWORD"))
        .child(field(
            p,
            &control.current_password,
            "profile-current",
            "Current password",
            "",
            |s| &mut s.control.current_password,
        ))
        .child(field(
            p,
            &control.next_password,
            "profile-new",
            "New password",
            "at least 8 characters",
            |s| &mut s.control.next_password,
        ))
        .child(field(
            p,
            &control.confirm_password,
            "profile-confirm",
            "Confirm new password",
            "",
            |s| &mut s.control.confirm_password,
        ));
    page = messages(page, state, p);
    page.child(
        row().pt(4.0).child(
            button(
                p,
                "profile-password-submit",
                if control.busy {
                    "Saving…"
                } else {
                    "Change password"
                },
                Look::Primary,
            )
            .on_press(State::change_own_password),
        ),
    )
    .child(divider(p))
    .child(row().child(crate::ui::bind_sign_out(button(
        p,
        "profile-sign-out",
        "Sign out",
        Look::Plain,
    ))))
}

// --- pieces -------------------------------------------------------------------

fn messages(page: El, state: &State, p: &Palette) -> El {
    let control = &state.control;
    if let Some(error) = &control.error {
        page.child(
            text(error.clone())
                .key("control-error")
                .w_full()
                .wrap()
                .font(DETAIL)
                .color(danger(p)),
        )
    } else if let Some(notice) = &control.notice {
        page.child(
            text(notice.clone())
                .key("control-notice")
                .w_full()
                .wrap()
                .font(DETAIL)
                .color(p.ink),
        )
    } else {
        page
    }
}

fn danger(p: &Palette) -> Color {
    if p.ink.r > 0.5 {
        rune::color::hex("#ff9b97").expect("valid colour")
    } else {
        rune::color::hex("#b42318").expect("valid colour")
    }
}

fn back(p: &Palette) -> El {
    row()
        .key("users-back")
        .role("button")
        .label("All users")
        .h(28.0)
        .px(6.0)
        .gap(4.0)
        .center_y()
        .radius(6.0)
        .hover(p.hover)
        .child(text("‹ All users").font(LABEL).color(p.muted))
        .on_press(State::back_to_accounts)
}

fn avatar(p: &Palette, account: &Account, size: f32) -> El {
    let (fill, ink) = if account.disabled {
        (p.surface_2, p.muted)
    } else {
        (p.accent, p.accent_ink)
    };
    el().size(size, size)
        .radius(size / 2.0)
        .fill(fill)
        .border(1.0, p.line.alpha(0.6))
        .center()
        .a11y_hidden()
        .child(
            text(initial(account.name()))
                .font(TextStyle::new(size * 0.4, 600.0))
                .color(ink),
        )
}

fn badge(p: &Palette, label: &'static str) -> El {
    el().h(20.0)
        .px(7.0)
        .center()
        .radius(10.0)
        .fill(p.surface_2)
        .border(1.0, p.line)
        .child(text(label).font(DETAIL).color(p.muted))
}

fn section(p: &Palette, label: &'static str) -> El {
    text(label).font(SECTION).color(p.muted).pt(4.0)
}

fn divider(p: &Palette) -> El {
    el().w_full().h(1.0).fill(p.line).a11y_hidden()
}

fn fact(p: &Palette, label: &str, value: &str) -> El {
    row()
        .w_full()
        .gap(8.0)
        .child(text(label.to_owned()).font(DETAIL).color(p.muted).w(104.0))
        .child(
            text(value.to_owned())
                .font(BODY)
                .color(p.ink)
                .grow(1.0)
                .min_w(0.0),
        )
}

#[derive(Clone, Copy)]
enum Look {
    Primary,
    Plain,
    Danger,
}

fn button(p: &Palette, key: &'static str, label: &str, look: Look) -> El {
    let (fill, ink) = match look {
        Look::Primary => (p.accent, p.accent_ink),
        Look::Plain => (p.surface_2, p.ink),
        Look::Danger => (danger(p).alpha(0.16), danger(p)),
    };
    row()
        .key(key)
        .role("button")
        .label(label.to_owned())
        .focusable(true)
        .h(34.0)
        .px(14.0)
        .center()
        .radius(7.0)
        .fill(fill)
        .border(
            1.0,
            if matches!(look, Look::Plain) {
                p.line
            } else {
                fill
            },
        )
        .hover(fill.mix(p.surface, 0.18))
        .child(text(label.to_owned()).font(BUTTON).color(ink))
}

fn switch(p: &Palette, key: &'static str, label: &'static str, on: bool) -> El {
    let knob = el()
        .size(16.0, 16.0)
        .radius(8.0)
        .fill(Color::WHITE)
        .absolute(if on { 18.0 } else { 2.0 }, 2.0);
    el().key(key)
        .role("switch")
        .label(label)
        .checked(on)
        .focusable(true)
        .size(36.0, 20.0)
        .radius(10.0)
        .fill(if on { p.accent } else { p.line })
        .child(knob)
}

/// A row of role choices; `action(role)` is what pressing one does.
fn segmented<F>(
    p: &Palette,
    key: &'static str,
    roles: &[Role],
    selected: Role,
    action: impl Fn(Role) -> F,
) -> El
where
    F: Fn(&mut State) + 'static,
{
    let mut group = row().key(key).role("radiogroup").gap(4.0).w_full();
    for &role in roles {
        let on = role == selected;
        group = group.child(
            row()
                .key(format!("{key}-{}", role_key(role)))
                .role("radio")
                .label(role.label())
                .checked(on)
                .h(32.0)
                .px(12.0)
                .center()
                .radius(7.0)
                .fill(if on { p.accent } else { p.surface_2 })
                .border(1.0, if on { p.accent } else { p.line })
                .hover(if on { p.accent } else { p.hover })
                .child(
                    text(role.label())
                        .font(BUTTON)
                        .color(if on { p.accent_ink } else { p.ink }),
                )
                .on_press(action(role)),
        );
    }
    group
}

fn role_key(role: Role) -> &'static str {
    match role {
        Role::SuperAdmin => "superadmin",
        Role::Admin => "admin",
        Role::User => "user",
    }
}

fn field(
    p: &Palette,
    model: &TextEdit,
    key: &'static str,
    label: &'static str,
    placeholder: &'static str,
    edit: fn(&mut State) -> &mut TextEdit,
) -> El {
    let input = text_input(model, BODY)
        .key(key)
        .label(label)
        .single_line()
        .w_full()
        .h(34.0)
        .px(10.0)
        .py(8.0)
        .radius(6.0)
        .fill(p.surface_2)
        .border(1.0, p.line)
        .color(p.ink)
        .placeholder(placeholder, p.muted)
        .on_edit(edit);
    let input = if model.is_secure() {
        input.secure()
    } else {
        input
    };
    col()
        .w_full()
        .gap(5.0)
        .child(text(label).font(LABEL).color(p.muted))
        .child(input)
}

fn when(secs: Option<i64>) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Some(secs) = secs.filter(|secs| *secs > 0) else {
        return "Never".to_owned();
    };
    let time = rune::time::LocalTime::at(secs * 1000);
    format!(
        "{} {} {}, {:02}:{:02}",
        time.day,
        MONTHS[(time.month.clamp(1, 12) - 1) as usize],
        time.year,
        time.hour,
        time.minute
    )
}

#[cfg(test)]
mod tests {
    use rune::{Env, Runtime, Tx};

    use super::*;
    use crate::config::Config;
    use crate::{SIZE, Screen};

    /// Signed in to the server as account `me` of the sample list, Control Panel open.
    fn signed_in_as(me: i64) -> Runtime<State> {
        let accounts = crate::sample_accounts();
        let mut state = State::new(Config::default());
        state.persist = false;
        state.session = accounts.iter().find(|a| a.id == me).cloned();
        state.signed_in = true;
        state.screen = Screen::Desktop;
        state.open_app(crate::desktop::AppId::Control);
        while state.inbox.rx.try_recv().is_ok() {}
        state.control.busy = false;
        state.control.loaded = true;
        state.control.accounts = accounts;
        let mut rt = Runtime::new(
            state,
            crate::ui::view,
            SIZE,
            Env {
                system_dark: true,
                scale: 2.0,
            },
        );
        rt.frame(0.0);
        rt
    }

    fn press(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt
            .find(key)
            .unwrap_or_else(|| panic!("`{key}` is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0..4 {
            rt.frame(1.0 / 60.0);
        }
    }

    fn shown(rt: &Runtime<State>, key: &str) -> bool {
        rt.find(key).is_some()
    }

    #[test]
    fn a_super_admin_changes_others_but_not_their_own_role() {
        let mut rt = signed_in_as(1);
        assert!(shown(&rt, "control-tab-users") && shown(&rt, "control-tab-profile"));
        assert!(shown(&rt, "user-4"), "every account is listed");

        press(&mut rt, "user-2");
        for key in ["user-role-superadmin", "user-role-admin", "user-role-user"] {
            assert!(shown(&rt, key), "{key}");
        }
        assert!(shown(&rt, "user-enabled") && shown(&rt, "user-delete"));

        press(&mut rt, "users-back");
        press(&mut rt, "user-1");
        assert!(!shown(&rt, "user-role-user"), "no changing your own role");
        assert!(!shown(&rt, "user-delete"), "no deleting yourself");
    }

    #[test]
    fn an_admin_manages_users_but_not_administrators() {
        let mut rt = signed_in_as(2);
        press(&mut rt, "user-1");
        assert!(!shown(&rt, "user-enabled") && !shown(&rt, "user-delete"));

        press(&mut rt, "users-back");
        press(&mut rt, "user-3");
        assert!(shown(&rt, "user-enabled") && shown(&rt, "user-delete"));
        assert!(!shown(&rt, "user-role-admin"), "admins can't promote");

        press(&mut rt, "users-back");
        press(&mut rt, "users-create");
        assert!(shown(&rt, "new-username"));
        assert!(!shown(&rt, "new-role"), "admins only create users");
    }

    #[test]
    fn a_user_only_has_a_profile() {
        let rt = signed_in_as(3);
        assert!(!shown(&rt, "control-tab-users"));
        assert!(shown(&rt, "profile-current") && shown(&rt, "profile-sign-out"));
    }

    #[test]
    fn without_a_server_session_control_panel_shows_the_connection() {
        let mut state = State::new(Config::default());
        state.persist = false;
        state.signed_in = true;
        state.screen = Screen::Desktop;
        state.open_app(crate::desktop::AppId::Control);
        assert_eq!(state.control.tab, Tab::Connection);
        assert!(state.inbox.rx.try_recv().is_err(), "no server calls");
    }

    #[test]
    fn deleting_asks_twice_and_replies_update_the_list() {
        let mut rt = signed_in_as(1);
        let state = &mut rt.state;
        state.open_account(3);
        state.delete_account(3);
        assert!(
            state.control.confirm_delete && !state.control.busy,
            "first press asks"
        );

        let mut tx = Tx::default();
        state.apply_reply(Reply::Deleted(3, Ok(())), &mut tx);
        assert_eq!(state.control.accounts.len(), 3);
        assert_eq!(state.control.page, Page::List);
        assert_eq!(state.control.notice.as_deref(), Some("Deleted bob."));

        let mut eve = state.control.accounts[2].clone();
        eve.id = 9;
        eve.username = "eve".to_owned();
        eve.role = Role::Admin;
        state.apply_reply(Reply::Created(Ok(eve)), &mut tx);
        let names: Vec<_> = state
            .control
            .accounts
            .iter()
            .map(|a| a.username.as_str())
            .collect();
        assert_eq!(
            names,
            ["root", "ada", "eve", "cat"],
            "sorted by role, then name"
        );

        state.apply_reply(
            Reply::Updated(Err(Failure {
                status: 403,
                message: "Only a Super Admin can do that.".to_owned(),
            })),
            &mut tx,
        );
        assert_eq!(
            state.control.error.as_deref(),
            Some("Only a Super Admin can do that.")
        );
        assert!(state.signed_in);
    }

    #[test]
    fn an_ended_session_signs_out() {
        let mut rt = signed_in_as(1);
        rt.state.apply_reply(
            Reply::Accounts(Err(Failure {
                status: 401,
                message: "Sign in again.".to_owned(),
            })),
            &mut Tx::default(),
        );
        assert!(!rt.state.signed_in);
    }

    #[test]
    fn new_accounts_are_checked_before_sending() {
        let mut rt = signed_in_as(1);
        let state = &mut rt.state;
        state.start_create_account();
        state.control.new_username = TextEdit::new("dora");
        state.control.new_password = TextEdit::new("short").secure();
        state.submit_create_account();
        assert!(state.control.error.is_some() && !state.control.busy);
        assert!(state.inbox.rx.try_recv().is_err(), "nothing sent");
    }
}
