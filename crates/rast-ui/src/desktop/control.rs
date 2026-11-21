//! Control Panel, laid out as classic RAST's: Profile and Appearance for everyone;
//! Users & Groups (Groups for Super Admins), Shared folders, Storage, Components and
//! Settings for administrators; natively, without a server session, the connection
//! settings. `shell` draws the sidebar and header. Lists stay calm (an icon, a name,
//! one line, one action); everything about one account or group is on its own page.

mod groups;
mod shell;
mod users;

use rune::{
    Color, El, Icon, ScrollSpec, TextEdit, TextStyle, Transition, Tx, col, el, row, text,
    text_input,
};

use super::{AppId, Palette, glyphs, icon, initial};
use crate::State;
use crate::server::{
    Access, Account, AccountChange, AppInfo, Choice, Failure, Group, NewAccount, Reply, Role,
    SessionInfo,
};

const TITLE: TextStyle = TextStyle::new(15.0, 600.0);
const BODY: TextStyle = TextStyle::new(13.0, 400.0);
const NAME: TextStyle = TextStyle::new(13.0, 500.0);
const DETAIL: TextStyle = TextStyle::new(11.5, 400.0);
const LABEL: TextStyle = TextStyle::new(12.0, 500.0);
const SECTION: TextStyle = TextStyle::new(10.5, 500.0).tracking(0.7);
const BUTTON: TextStyle = TextStyle::new(12.5, 500.0);

/// What Control Panel shows. `Users` and `Groups` are the two views of one section,
/// *Users & Groups*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Profile,
    /// Personalize's cards, for everyone.
    Appearance,
    Users,
    Groups,
    Shares,
    Storage,
    Components,
    Settings,
    Connection,
}

impl Tab {
    /// The sidebar entry's key. Groups has none of its own: it is a view of Users.
    fn key(self) -> &'static str {
        match self {
            Self::Profile => "control-tab-profile",
            Self::Appearance => "control-tab-appearance",
            Self::Users | Self::Groups => "control-tab-users",
            Self::Shares => "control-tab-shares",
            Self::Storage => "control-tab-storage",
            Self::Components => "control-tab-components",
            Self::Settings => "control-tab-settings",
            Self::Connection => "control-tab-connection",
        }
    }

    /// The section's name: in the sidebar, the header and the window's title.
    fn label(self) -> &'static str {
        match self {
            Self::Profile => "Profile",
            Self::Appearance => "Appearance",
            Self::Users | Self::Groups => "Users & Groups",
            Self::Shares => "Shared folders",
            Self::Storage => "Storage",
            Self::Components => "Components",
            Self::Settings => "Settings",
            Self::Connection => "Connection",
        }
    }

    /// The line under the header's name, worded as classic RAST worded it.
    fn detail(self) -> &'static str {
        match self {
            Self::Profile => "Your account, password and active sessions",
            Self::Appearance => "",
            Self::Users | Self::Groups => "Accounts, groups, sign-in activity and access rules",
            Self::Shares => "Shared folders and access permissions",
            Self::Storage => "Storage locations, capacity and per-user usage",
            Self::Components => "System health, host resources and database",
            Self::Settings => "Server-wide preferences and read-only configuration",
            Self::Connection => "The RAST server this desktop connects to",
        }
    }

    fn glyph(self) -> &'static Icon {
        match self {
            Self::Profile => &glyphs::USER,
            Self::Appearance => &glyphs::WALLPAPER,
            Self::Users | Self::Groups => &glyphs::USERS,
            Self::Shares => &glyphs::SHARE,
            Self::Storage => &glyphs::DISK,
            Self::Components => &glyphs::ACTIVITY,
            Self::Settings => &glyphs::SETTINGS,
            Self::Connection => &glyphs::NETWORK,
        }
    }

    /// The sidebar entry this shows under.
    fn section(self) -> Tab {
        match self {
            Self::Groups => Self::Users,
            other => other,
        }
    }
}

/// A popup open over the Users table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Popup {
    /// The actions menu of account `id`.
    Actions(i64),
    /// Its menu again, asking for a second press to disable the account.
    ConfirmDisable(i64),
    /// The rows-per-page choices.
    PerPage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    List,
    Create,
    Account(i64),
    /// An account's groups, rules and what it may open.
    Access(i64),
    CreateGroup,
    Group(i64),
}

pub(crate) struct ControlPanel {
    pub(crate) tab: Tab,
    pub(crate) page: Page,
    pub(crate) accounts: Vec<Account>,
    pub(crate) loaded: bool,
    loading_accounts: bool,
    pub(crate) groups: Vec<Group>,
    pub(crate) apps: Vec<AppInfo>,
    pub(crate) groups_loaded: bool,
    loading_groups: bool,
    /// The account whose access page is open, once loaded.
    pub(crate) access: Option<(i64, Access)>,
    /// A change is on its way to the server.
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
    pub(crate) group_name: TextEdit,
    pub(crate) group_description: TextEdit,
    pub(crate) search: TextEdit,
    /// How many accounts the table shows a page.
    pub(crate) per_page: usize,
    /// The table's page, from 0. Editing the search goes back to the first.
    pub(crate) list_page: usize,
    pub(crate) popup: Option<Popup>,
    /// Your own sessions (Profile), once loaded.
    pub(crate) sessions: Option<Vec<SessionInfo>>,
    /// The open account's sessions, once loaded.
    pub(crate) account_sessions: Option<(i64, Vec<SessionInfo>)>,
    /// The display name being edited: yours on Profile, an account's on its page.
    pub(crate) display_name: TextEdit,
    /// A new account chooses its own password at the first sign-in.
    pub(crate) new_must_change: bool,
    /// What to say when the change on its way succeeds, instead of "Saved."
    pending_notice: Option<String>,
}

impl Default for ControlPanel {
    fn default() -> Self {
        Self {
            tab: Tab::Connection,
            page: Page::List,
            accounts: Vec::new(),
            loaded: false,
            loading_accounts: false,
            groups: Vec::new(),
            apps: Vec::new(),
            groups_loaded: false,
            loading_groups: false,
            access: None,
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
            group_name: TextEdit::new(""),
            group_description: TextEdit::new(""),
            search: TextEdit::new(""),
            per_page: users::DEFAULT_PER_PAGE,
            list_page: 0,
            popup: None,
            sessions: None,
            account_sessions: None,
            display_name: TextEdit::new(""),
            new_must_change: true,
            pending_notice: None,
        }
    }
}

/// The sidebar's sections, in classic RAST's order.
fn tabs(state: &State) -> Vec<Tab> {
    match &state.session {
        Some(session) if session.role.is_admin() => vec![
            Tab::Profile,
            Tab::Appearance,
            Tab::Users,
            Tab::Shares,
            Tab::Storage,
            Tab::Components,
            Tab::Settings,
        ],
        Some(_) => vec![Tab::Profile, Tab::Appearance],
        None => vec![Tab::Connection, Tab::Appearance],
    }
}

/// Super Admins also see the Groups view of *Users & Groups*.
fn sees_groups(state: &State) -> bool {
    state
        .session
        .as_ref()
        .is_some_and(|session| session.role == Role::SuperAdmin)
}

/// What Control Panel shows: the chosen tab while this account may see it, else the first.
fn current(state: &State) -> Tab {
    let tab = state.control.tab;
    let tabs = tabs(state);
    if tabs.contains(&tab) || (tab == Tab::Groups && sees_groups(state)) {
        tab
    } else {
        tabs[0]
    }
}

/// The section Control Panel shows, which its window's title names.
pub(super) fn page_title(state: &State) -> &'static str {
    current(state).label()
}

// --- actions ------------------------------------------------------------------

impl State {
    /// Opening Control Panel lands on Profile, as on classic RAST (on the connection
    /// settings without a server session).
    pub(crate) fn reset_control(&mut self) {
        let tab = tabs(self)[0];
        self.control = ControlPanel::default();
        self.open_control_tab(tab);
    }

    pub(crate) fn open_control_tab(&mut self, tab: Tab) {
        if self.control.tab == Tab::Appearance && !self.desktop.look.dialog {
            // Leaving the page puts back the saved look.
            self.discard_appearance();
        }
        self.control.tab = tab;
        self.control.page = Page::List;
        self.control.popup = None;
        self.control.error = None;
        self.control.notice = None;
        match tab {
            Tab::Users => self.load_accounts(),
            Tab::Groups => {
                self.load_accounts();
                self.load_groups();
            }
            Tab::Profile => {
                let name = self
                    .session
                    .as_ref()
                    .map(|me| me.display_name.clone())
                    .unwrap_or_default();
                self.control.display_name = TextEdit::new(name);
                self.control.sessions = None;
                self.calls().my_sessions();
            }
            Tab::Appearance => self.desktop.look.sync_hex(),
            Tab::Shares | Tab::Storage | Tab::Components | Tab::Settings | Tab::Connection => {}
        }
    }

    fn load_accounts(&mut self) {
        if !self.control.loaded && !self.control.loading_accounts {
            self.control.loading_accounts = true;
            self.calls().accounts();
        }
    }

    fn load_groups(&mut self) {
        if !self.control.groups_loaded && !self.control.loading_groups {
            self.control.loading_groups = true;
            self.calls().groups();
        }
    }

    /// The signed-in account's own access arrived: hide what it may not open.
    fn set_my_access(&mut self, access: Access) {
        self.access = Some(access);
        if !self.may_open(AppId::Control.access_id())
            && self.desktop.windows.get(AppId::Control).is_some()
        {
            self.close_window(AppId::Control);
        }
    }

    pub(crate) fn open_account(&mut self, id: i64) {
        let control = &mut self.control;
        let name = control
            .accounts
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.display_name.clone())
            .unwrap_or_default();
        control.page = Page::Account(id);
        control.popup = None;
        control.reset_password = TextEdit::new("").secure();
        control.display_name = TextEdit::new(name);
        control.account_sessions = None;
        control.confirm_delete = false;
        control.error = None;
        control.notice = None;
        if self.manages(id) {
            self.calls().user_sessions(id);
        }
    }

    /// May the signed-in account change account `id` (not itself)?
    fn manages(&self, id: i64) -> bool {
        let (Some(me), Some(account)) = (
            &self.session,
            self.control.accounts.iter().find(|a| a.id == id),
        ) else {
            return false;
        };
        me.id != id && me.role.may_manage(account.role)
    }

    /// Back to the list. What was said about the page you leave goes with it.
    pub(crate) fn back_to_accounts(&mut self) {
        self.control.page = Page::List;
        self.control.popup = None;
        self.control.error = None;
        self.control.notice = None;
        self.control.confirm_delete = false;
    }

    pub(crate) fn start_create_account(&mut self) {
        let control = &mut self.control;
        control.page = Page::Create;
        control.popup = None;
        control.new_username = TextEdit::new("");
        control.new_display = TextEdit::new("");
        control.new_password = TextEdit::new("").secure();
        control.new_role = Role::User;
        control.new_must_change = true;
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
            must_change_password: self.control.new_must_change,
        };
        self.control.busy = true;
        self.control.error = None;
        self.calls().create(&account);
    }

    fn change_account(&mut self, id: i64, change: AccountChange) {
        self.change_account_saying(id, change, None);
    }

    /// Send `change`; on success say `notice` instead of "Saved."
    fn change_account_saying(&mut self, id: i64, change: AccountChange, notice: Option<String>) {
        if self.control.busy {
            return;
        }
        self.control.busy = true;
        self.control.error = None;
        self.control.notice = None;
        self.control.pending_notice = notice;
        self.calls().update(id, &change);
    }

    pub(crate) fn save_account_name(&mut self, id: i64) {
        let name = self.control.display_name.text.trim().to_owned();
        self.change_account(
            id,
            AccountChange {
                display_name: Some(name),
                ..AccountChange::default()
            },
        );
    }

    /// Require (or stop requiring) a new password at the next sign-in.
    pub(crate) fn set_must_change(&mut self, id: i64, on: bool) {
        self.change_account_saying(
            id,
            AccountChange {
                must_change_password: Some(on),
                ..AccountChange::default()
            },
            Some(
                if on {
                    "They choose a new password at their next sign-in, and were signed out."
                } else {
                    "They keep their current password."
                }
                .to_owned(),
            ),
        );
    }

    pub(crate) fn unlock_account(&mut self, id: i64) {
        self.change_account_saying(
            id,
            AccountChange {
                unlock: true,
                ..AccountChange::default()
            },
            Some("Unlocked. They can sign in again.".to_owned()),
        );
    }

    /// Sign an account out on every device.
    pub(crate) fn sign_out_account(&mut self, id: i64) {
        if self.control.busy {
            return;
        }
        self.control.busy = true;
        self.control.error = None;
        self.control.notice = None;
        self.control.pending_notice = Some("Signed out everywhere.".to_owned());
        self.calls().end_user_sessions(id);
    }

    pub(crate) fn save_my_name(&mut self) {
        if self.control.busy {
            return;
        }
        let name = self.control.display_name.text.trim().to_owned();
        self.control.busy = true;
        self.control.error = None;
        self.control.notice = None;
        self.calls().save_profile(&name);
    }

    pub(crate) fn end_my_session(&mut self, id: &str) {
        if self.control.busy {
            return;
        }
        self.control.busy = true;
        self.control.error = None;
        self.control.notice = None;
        self.control.pending_notice = Some("Signed out of that session.".to_owned());
        self.calls().end_my_session(id);
    }

    pub(crate) fn end_my_other_sessions(&mut self) {
        if self.control.busy {
            return;
        }
        self.control.busy = true;
        self.control.error = None;
        self.control.notice = None;
        self.control.pending_notice = Some("Signed out everywhere else.".to_owned());
        self.calls().end_my_other_sessions();
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

    /// A temporary password: they choose their own at the next sign-in.
    pub(crate) fn reset_account_password(&mut self, id: i64) {
        let password = self.control.reset_password.text.clone();
        if password.chars().count() < 8 {
            self.control.error = Some("Use at least 8 characters for the new password.".to_owned());
            return;
        }
        self.change_account_saying(
            id,
            AccountChange {
                password: Some(password),
                must_change_password: Some(true),
                ..AccountChange::default()
            },
            Some("Password set. They choose their own at the next sign-in.".to_owned()),
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
        let failure = match reply {
            Reply::Accounts(result) => {
                self.control.loading_accounts = false;
                result
                    .map(|accounts| {
                        self.control.accounts = accounts;
                        self.control.loaded = true;
                    })
                    .err()
            }
            Reply::Groups(result) => {
                self.control.loading_groups = false;
                result
                    .map(|(groups, apps)| {
                        self.control.groups = groups;
                        self.control.apps = apps;
                        self.control.groups_loaded = true;
                    })
                    .err()
            }
            Reply::MyAccess(result) => match result {
                Ok(access) => {
                    self.set_my_access(access);
                    None
                }
                // Keep what is known; only an ended session matters here.
                Err(failure) => failure.signed_out().then_some(failure),
            },
            Reply::UserAccess(id, result) => {
                let saved = std::mem::take(&mut self.control.busy);
                result
                    .map(|access| {
                        if saved {
                            self.control.notice = Some("Saved.".to_owned());
                        }
                        // Keep the Groups tab in step with memberships changed here.
                        for group in &mut self.control.groups {
                            group.member_ids.retain(|member| *member != id);
                            if access.group_ids.contains(&group.id) {
                                group.member_ids.push(id);
                                group.member_ids.sort_unstable();
                            }
                        }
                        self.control.access = Some((id, access));
                    })
                    .err()
            }
            Reply::Created(result) => {
                self.control.busy = false;
                result
                    .map(|account| {
                        self.control.notice = Some(format!("Created {}.", account.username));
                        self.control.accounts.push(account);
                        sort(&mut self.control.accounts);
                        self.control.page = Page::List;
                    })
                    .err()
            }
            Reply::Updated(result) => {
                self.control.busy = false;
                let notice = self.control.pending_notice.take();
                result
                    .map(|account| {
                        self.control.reset_password = TextEdit::new("").secure();
                        self.control.notice = Some(notice.unwrap_or_else(|| "Saved.".to_owned()));
                        let id = account.id;
                        if let Some(slot) = self.control.accounts.iter_mut().find(|a| a.id == id) {
                            *slot = account;
                        }
                        sort(&mut self.control.accounts);
                        // A reset or a sign-in turned off ends sessions on the open page.
                        if self.control.page == Page::Account(id) && self.manages(id) {
                            self.calls().user_sessions(id);
                        }
                    })
                    .err()
            }
            Reply::MySessions(result, changed) => {
                let notice = if changed {
                    self.control.busy = false;
                    self.control.pending_notice.take()
                } else {
                    None
                };
                result
                    .map(|sessions| {
                        if let Some(notice) = notice {
                            self.control.notice = Some(notice);
                        }
                        self.control.sessions = Some(sessions);
                    })
                    .err()
            }
            Reply::UserSessions(id, result, changed) => {
                let notice = if changed {
                    self.control.busy = false;
                    self.control.pending_notice.take()
                } else {
                    None
                };
                result
                    .map(|sessions| {
                        if let Some(notice) = notice {
                            self.control.notice = Some(notice);
                        }
                        self.control.account_sessions = Some((id, sessions));
                    })
                    .err()
            }
            Reply::ProfileSaved(result) => {
                self.control.busy = false;
                result
                    .map(|account| {
                        self.control.display_name = TextEdit::new(account.display_name.clone());
                        self.control.notice = Some("Saved.".to_owned());
                        if let Some(slot) = self
                            .control
                            .accounts
                            .iter_mut()
                            .find(|a| a.id == account.id)
                        {
                            *slot = account.clone();
                        }
                        if self.session.as_ref().is_some_and(|me| me.id == account.id) {
                            self.session = Some(account);
                        }
                    })
                    .err()
            }
            Reply::Deleted(id, result) => {
                self.control.busy = false;
                result
                    .map(|()| {
                        if let Some(gone) = self.control.accounts.iter().find(|a| a.id == id) {
                            self.control.notice = Some(format!("Deleted {}.", gone.username));
                        }
                        self.control.accounts.retain(|a| a.id != id);
                        for group in &mut self.control.groups {
                            group.member_ids.retain(|member| *member != id);
                        }
                        self.control.page = Page::List;
                        self.control.confirm_delete = false;
                    })
                    .err()
            }
            Reply::PasswordChanged(result) => {
                self.control.busy = false;
                result
                    .map(|()| {
                        self.control.current_password = TextEdit::new("").secure();
                        self.control.next_password = TextEdit::new("").secure();
                        self.control.confirm_password = TextEdit::new("").secure();
                        self.control.notice = Some(
                            "Password changed. Other sessions of your account were signed out."
                                .to_owned(),
                        );
                    })
                    .err()
            }
            Reply::GroupCreated(result) => {
                self.control.busy = false;
                result.map(|group| self.group_created(group)).err()
            }
            Reply::GroupSaved(result) => {
                self.control.busy = false;
                result.map(|group| self.group_saved(group)).err()
            }
            Reply::GroupDeleted(id, result) => {
                self.control.busy = false;
                result.map(|()| self.group_deleted(id)).err()
            }
            Reply::Appearance(result) => self.appearance_loaded(result),
            Reply::AppearanceSaved(result) => self.appearance_saved(result),
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
        self.control.pending_notice = None;
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

/// Control Panel, `width` points wide inside its window's border: the sections on the left,
/// the open one's header, and its page, which scrolls. `compact` lays pages out for a phone.
pub(super) fn view(state: &State, p: &Palette, width: f32, compact: bool) -> El {
    let tab = current(state);
    let margin = shell::margin(width);
    let narrow = shell::narrow(width);
    // The room a page has beside the sidebar, inside the margins.
    let room = (width - shell::nav_width(width) - 2.0 * margin).max(0.0);
    let page = match (tab, &state.session) {
        (Tab::Users, Some(me)) => users_page(state, p, me, compact, room),
        (Tab::Groups, Some(_)) => groups::page(state, p, compact),
        (Tab::Profile, Some(me)) => profile_page(state, p, me, compact),
        (Tab::Appearance, _) => super::personalize::page(state, p, room, width),
        (Tab::Shares | Tab::Storage | Tab::Components | Tab::Settings, Some(_)) => {
            shell::not_yet(p, tab)
        }
        _ => crate::ui::settings_body(state),
    };
    // Keyed by the page, so each page scrolls on its own: an account, the list or another
    // section opens at its top rather than where the last page was left. Pages swap at once,
    // as classic RAST's did, rather than fading over each other.
    let content = col()
        .key(format!("control-page-{tab:?}-{:?}", state.control.page))
        .transition(Transition::none())
        .w_full()
        .grow(1.0)
        .min_w(0.0)
        .min_h(0.0)
        .child(
            scroller().child(
                col()
                    .w_full()
                    .px(margin)
                    .pt(if narrow { margin } else { 18.0 })
                    .pb(if narrow { margin } else { 26.0 })
                    .gap(12.0)
                    .child(page),
            ),
        );
    let listing = state.control.page == Page::List;
    let action = match tab {
        Tab::Users if listing => Some(shell::action(
            p,
            "users-create",
            "Add user",
            State::start_create_account,
        )),
        Tab::Groups if listing => Some(shell::action(
            p,
            "groups-create",
            "Create group",
            State::start_create_group,
        )),
        _ => None,
    };
    row()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(shell::nav(state, p, tab, width))
        .child(
            col()
                .key("control-main")
                .grow(1.0)
                .min_w(0.0)
                .min_h(0.0)
                .child(shell::header(p, tab, action, width))
                .child(content),
        )
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

/// *Users*: the table, an account, its access, or a new account. `width` is the page's room.
fn users_page(state: &State, p: &Palette, me: &Account, compact: bool, width: f32) -> El {
    let control = &state.control;
    let account = |id: i64| control.accounts.iter().find(|a| a.id == id);
    match control.page {
        Page::Create => create_page(state, p, me),
        Page::Account(id) => match account(id) {
            Some(account) => account_page(state, p, me, account, compact),
            None => users::list(state, p, me, compact, width),
        },
        Page::Access(id) => match account(id) {
            Some(account) => groups::access_page(state, p, me, account, compact),
            None => users::list(state, p, me, compact, width),
        },
        Page::List | Page::CreateGroup | Page::Group(_) => {
            users::list(state, p, me, compact, width)
        }
    }
}

/// Control Panel's popups (the Users table's menus), for the desktop to draw over its windows
/// so the window doesn't clip them.
pub(super) fn popups(state: &State, p: &Palette) -> Vec<El> {
    users::popups(state, p)
}

fn account_page(state: &State, p: &Palette, me: &Account, account: &Account, compact: bool) -> El {
    let control = &state.control;
    let id = account.id;
    let yourself = account.id == me.id;
    let manage = !yourself && me.role.may_manage(account.role);
    let header = row()
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
        );
    let mut page = col()
        .w_full()
        .max_w(560.0)
        .gap(12.0)
        .child(back(p))
        .child(header_message(header, state, p, compact));

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
        page = page.child(section(p, "NAME")).child(
            row()
                .w_full()
                .gap(8.0)
                .child(
                    field(
                        p,
                        &control.display_name,
                        "user-display-name",
                        "Display name",
                        "e.g. Alice Smith",
                        |s| &mut s.control.display_name,
                    )
                    .grow(1.0)
                    .min_w(0.0),
                )
                .child(
                    col().child(el().h(18.0)).child(
                        button(p, "user-name-save", "Save", Look::Plain)
                            .on_press(move |s: &mut State| s.save_account_name(id)),
                    ),
                ),
        );

        let enabled = !account.disabled;
        page = page
            .child(section(p, "SIGN-IN"))
            .child(setting(
                p,
                if enabled { "Can sign in" } else { "Disabled" },
                "Disabling signs the account out everywhere.",
                switch(p, "user-enabled", "Can sign in", enabled)
                    .on_press(move |s: &mut State| s.set_account_disabled(id, enabled)),
            ))
            .child(setting(
                p,
                "New password at next sign-in",
                if account.must_change_password {
                    "They choose their own password before they continue."
                } else {
                    "Turning this on also signs them out."
                },
                switch(
                    p,
                    "user-must-change",
                    "New password at next sign-in",
                    account.must_change_password,
                )
                .on_press({
                    let on = !account.must_change_password;
                    move |s: &mut State| s.set_must_change(id, on)
                }),
            ));
        if let Some(until) = account.locked_until {
            page = page.child(setting(
                p,
                &locked_line(until),
                "Too many wrong passwords. It unlocks by itself, or unlock it now.",
                button(p, "user-unlock", "Unlock", Look::Plain)
                    .on_press(move |s: &mut State| s.unlock_account(id)),
            ));
        } else if account.failed_attempts > 0 {
            page = page.child(
                text(match account.failed_attempts {
                    1 => "1 wrong password recently.".to_owned(),
                    n => format!("{n} wrong passwords recently."),
                })
                .key("user-failures")
                .font(DETAIL)
                .color(p.muted),
            );
        }

        page = page
            .child(section(p, "PASSWORD").key("user-password-title"))
            .child(
                row()
                    .w_full()
                    .gap(8.0)
                    .child(
                        field(
                            p,
                            &control.reset_password,
                            "user-new-password",
                            "Temporary password",
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
            )
            .child(
                text("They choose their own password at the next sign-in. Setting one signs them out.")
                    .font(DETAIL)
                    .color(p.muted)
                    .w_full()
                    .wrap(),
            );

        page = page.child(section(p, "SESSIONS").key("user-sessions-title"));
        page = match &control.account_sessions {
            Some((owner, sessions)) if *owner == id => {
                let mut list = col().key("user-sessions").w_full().gap(10.0);
                for session in sessions {
                    list = list.child(session_row(p, session, None));
                }
                if sessions.is_empty() {
                    list = list.child(text("Not signed in anywhere.").font(DETAIL).color(p.muted));
                }
                page = page.child(list);
                if sessions.is_empty() {
                    page
                } else {
                    page.child(
                        row().child(
                            button(p, "user-sign-out", "Sign out everywhere", Look::Plain)
                                .on_press(move |s: &mut State| s.sign_out_account(id)),
                        ),
                    )
                }
            }
            _ => page.child(text("Loading…").font(DETAIL).color(p.muted)),
        };
    }

    page = page
        .child(section(p, "ACCESS"))
        .child(
            link_row(
                state,
                p,
                "user-access",
                "groups",
                "Apps & groups",
                if account.role.is_admin() {
                    "Administrators open every app"
                } else {
                    "Groups, app rules and what this account can open"
                },
            )
            .on_press(move |s: &mut State| s.open_access(id)),
        )
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
    page = page.child(section(p, "SIGN-IN")).child(setting(
        p,
        "New password at first sign-in",
        "They replace the password you give them before they continue.",
        switch(
            p,
            "new-must-change",
            "New password at first sign-in",
            control.new_must_change,
        )
        .on_press(|s: &mut State| s.control.new_must_change = !s.control.new_must_change),
    ));
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

fn profile_page(state: &State, p: &Palette, me: &Account, compact: bool) -> El {
    let control = &state.control;
    let header = row()
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
        );
    let intro = shell::intro(
        p,
        Some("Your profile"),
        "Update how you appear on RAST, change your password and review where you are \
         signed in. Wallpaper and theme live in the Appearance section.",
    );
    let page = col()
        .key("profile")
        .w_full()
        .max_w(560.0)
        .gap(12.0)
        .child(header_message(header, state, p, compact))
        .child(section(p, "YOUR APPS"))
        .child(my_apps(state, p, me))
        .child(section(p, "NAME"))
        .child(
            row()
                .w_full()
                .gap(8.0)
                .child(
                    field(
                        p,
                        &control.display_name,
                        "profile-name",
                        "Display name",
                        "e.g. Alice Smith",
                        |s| &mut s.control.display_name,
                    )
                    .grow(1.0)
                    .min_w(0.0),
                )
                .child(
                    col().child(el().h(18.0)).child(
                        button(p, "profile-name-save", "Save", Look::Plain)
                            .on_press(State::save_my_name),
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
        ))
        .child(
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
        .child(section(p, "SESSIONS"))
        .child(my_sessions(p, control.sessions.as_deref()));
    let page = page
        .child(divider(p))
        .child(row().child(crate::ui::bind_sign_out(button(
            p,
            "profile-sign-out",
            "Sign out",
            Look::Plain,
        ))));
    col().w_full().gap(12.0).child(intro).child(page)
}

/// Where you are signed in; every session but this one can be ended here.
fn my_sessions(p: &Palette, sessions: Option<&[SessionInfo]>) -> El {
    let Some(sessions) = sessions else {
        return text("Loading…").font(DETAIL).color(p.muted);
    };
    let mut list = col().key("my-sessions").w_full().gap(10.0);
    for session in sessions {
        let end = (!session.current).then(|| {
            let id = session.id.clone();
            button(p, format!("session-end-{id}"), "Sign out", Look::Plain)
                .on_press(move |s: &mut State| s.end_my_session(&id))
        });
        list = list.child(session_row(p, session, end));
    }
    if sessions.iter().any(|session| !session.current) {
        list = list.child(
            row().child(
                button(
                    p,
                    "sessions-end-others",
                    "Sign out everywhere else",
                    Look::Plain,
                )
                .on_press(State::end_my_other_sessions),
            ),
        );
    } else {
        list = list.child(
            text("You are signed in only here.")
                .font(DETAIL)
                .color(p.muted),
        );
    }
    list
}

/// One signed-in device: what it is, where, and when it was last used.
fn session_row(p: &Palette, session: &SessionInfo, end: Option<El>) -> El {
    let mut name = row().gap(6.0).center_y().min_w(0.0).child(
        text(session.device.clone())
            .font(NAME)
            .color(p.ink)
            .truncate(),
    );
    if session.current {
        name = name.child(badge(p, "This device"));
    }
    let mut facts = Vec::new();
    if !session.ip.is_empty() {
        facts.push(session.ip.clone());
    }
    if !session.current {
        facts.push(ago(session.last_seen_at));
    }
    facts.push(format!("since {}", when(Some(session.created_at))));
    let mut item = row().w_full().gap(10.0).center_y().child(
        col().grow(1.0).min_w(0.0).gap(2.0).child(name).child(
            text(facts.join(" · "))
                .font(DETAIL)
                .color(p.muted)
                .truncate(),
        ),
    );
    if let Some(end) = end {
        item = item.child(end);
    }
    item
}

// --- pieces -------------------------------------------------------------------

/// The latest error, else the latest notice.
fn message(state: &State, p: &Palette) -> Option<El> {
    let control = &state.control;
    if let Some(error) = &control.error {
        Some(
            text(error.clone())
                .key("control-error")
                .wrap()
                .font(DETAIL)
                .color(danger(p)),
        )
    } else {
        control.notice.as_ref().map(|notice| {
            text(notice.clone())
                .key("control-notice")
                .wrap()
                .font(DETAIL)
                .color(p.ink)
        })
    }
}

/// The message on its own line under the page's heading.
fn messages(page: El, state: &State, p: &Palette) -> El {
    match message(state, p) {
        Some(line) => page.child(line.w_full()),
        None => page,
    }
}

/// The message at the end of a page's header row. Pages with many controls
/// use it so that *Saved.* appears without moving the controls under the
/// pointer.
fn header_message(header: El, state: &State, p: &Palette, compact: bool) -> El {
    match message(state, p) {
        Some(line) => header.child(line.max_w(if compact { 140.0 } else { 300.0 })),
        None => header,
    }
}

fn danger(p: &Palette) -> Color {
    if p.ink.r > 0.5 {
        rune::color::hex("#ff9b97").expect("valid colour")
    } else {
        rune::color::hex("#b42318").expect("valid colour")
    }
}

/// What the signed-in account may open, and why.
fn my_apps(state: &State, p: &Palette, me: &Account) -> El {
    if me.role.is_admin() {
        return text("Administrators open every app.")
            .font(DETAIL)
            .color(p.muted);
    }
    let Some(access) = &state.access else {
        return text("Loading…").font(DETAIL).color(p.muted);
    };
    let mut list = col().key("my-apps").w_full().gap(8.0);
    for decision in &access.apps {
        list = list.child(app_row(
            p,
            &decision.label,
            decision.reason(),
            Some(decision.allowed),
            None,
        ));
    }
    let groups: Vec<&str> = access.groups.iter().map(|g| g.name.as_str()).collect();
    list.child(
        text(if groups.is_empty() {
            "You are not in any group.".to_owned()
        } else {
            format!("Your groups: {}", groups.join(", "))
        })
        .font(DETAIL)
        .color(p.muted)
        .w_full()
        .wrap(),
    )
}

fn back(p: &Palette) -> El {
    back_link(p, "users-back", "All users").on_press(State::back_to_accounts)
}

fn back_link(p: &Palette, key: &'static str, label: &str) -> El {
    row()
        .key(key)
        .role("button")
        .label(label.to_owned())
        .h(28.0)
        .px(6.0)
        .gap(4.0)
        .center_y()
        .radius(6.0)
        .hover(p.hover)
        .child(text(format!("‹ {label}")).font(LABEL).color(p.muted))
}

/// A row that opens a page: icon, title, one line, chevron.
fn link_row(
    state: &State,
    p: &Palette,
    key: &'static str,
    icon_name: &str,
    title: &str,
    line: &str,
) -> El {
    row()
        .key(key)
        .role("button")
        .label(title.to_owned())
        .w_full()
        .h(50.0)
        .px(10.0)
        .gap(10.0)
        .center_y()
        .radius(8.0)
        .fill(p.surface_2.alpha(0.5))
        .border(1.0, p.line.alpha(0.7))
        .hover(p.hover)
        .child(tile(state, p, icon_name, 32.0))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(text(title.to_owned()).font(NAME).color(p.ink).truncate())
                .child(text(line.to_owned()).font(DETAIL).color(p.muted).truncate()),
        )
        .child(text("›").font(BODY).color(p.muted).a11y_hidden())
}

/// An icon on a small rounded square.
fn tile(state: &State, p: &Palette, icon_name: &str, size: f32) -> El {
    el().size(size, size)
        .radius(size / 4.0)
        .fill(p.surface_2)
        .border(1.0, p.line)
        .center()
        .a11y_hidden()
        .child(icon(state, icon_name, size * 0.56))
}

/// "Allowed" or "Denied".
fn verdict(p: &Palette, allowed: bool) -> El {
    let (label, color) = if allowed {
        ("Allowed", allowed_color(p))
    } else {
        ("Denied", danger(p))
    };
    el().h(18.0)
        .px(7.0)
        .center()
        .radius(9.0)
        .fill(color.alpha(0.16))
        .child(text(label).font(TextStyle::new(10.5, 600.0)).color(color))
}

fn allowed_color(p: &Palette) -> Color {
    if p.ink.r > 0.5 {
        rune::color::hex("#66daa3").expect("valid colour")
    } else {
        rune::color::hex("#1d8a60").expect("valid colour")
    }
}

/// One app: its name, a line about it, optionally its verdict and a control.
fn app_row(
    p: &Palette,
    label: &str,
    line: String,
    allowed: Option<bool>,
    control: Option<El>,
) -> El {
    let mut name = row()
        .gap(6.0)
        .center_y()
        .child(text(label.to_owned()).font(NAME).color(p.ink));
    if let Some(allowed) = allowed {
        name = name.child(verdict(p, allowed));
    }
    let info = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(2.0)
        .child(name)
        .child(text(line).font(DETAIL).color(p.muted).w_full().wrap());
    let mut item = row().w_full().gap(10.0).center_y().child(info);
    if let Some(control) = control {
        item = item.child(control);
    }
    item
}

/// Inherit / Allow / Deny for one app; `action(choice)` applies a choice.
fn choice_bar<F>(
    p: &Palette,
    key: String,
    label: &str,
    selected: Choice,
    action: impl Fn(Choice) -> F,
) -> El
where
    F: Fn(&mut State) + 'static,
{
    let mut bar = row()
        .key(key.clone())
        .role("radiogroup")
        .label(label.to_owned())
        .gap(2.0)
        .padding(2.0)
        .radius(8.0)
        .fill(p.surface_2)
        .border(1.0, p.line);
    for choice in Choice::ALL {
        let on = choice == selected;
        let (fill, ink) = match choice {
            _ if !on => (None, p.muted),
            Choice::Inherit => (Some(p.hover), p.ink),
            Choice::Allow => (Some(p.accent), p.accent_ink),
            Choice::Deny => (Some(danger(p).alpha(0.2)), danger(p)),
        };
        let mut item = row()
            .key(format!("{key}-{}", choice.key()))
            .role("radio")
            .label(choice.label())
            .checked(on)
            .focusable(true)
            .h(26.0)
            .px(10.0)
            .center()
            .radius(6.0)
            .child(
                text(choice.label())
                    .font(TextStyle::new(12.0, 500.0))
                    .color(ink),
            )
            .on_press(action(choice));
        item = match fill {
            Some(fill) => item.fill(fill),
            None => item.hover(p.hover),
        };
        bar = bar.child(item);
    }
    bar
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

/// A setting: what it is, one line about it, and its control on the right.
fn setting(p: &Palette, title: &str, line: &str, control: El) -> El {
    row()
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(text(title.to_owned()).font(BODY).color(p.ink))
                .child(
                    text(line.to_owned())
                        .font(DETAIL)
                        .color(p.muted)
                        .w_full()
                        .wrap(),
                ),
        )
        .child(control)
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

fn button(p: &Palette, key: impl Into<String>, label: &str, look: Look) -> El {
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

fn switch(p: &Palette, key: impl Into<String>, label: impl Into<String>, on: bool) -> El {
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

/// Seconds since the Unix epoch.
fn now_secs() -> i64 {
    rune::time::system_now()
        .duration_since(rune::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// "Active now", "5 min ago", "3 h ago", "Yesterday", "4 days ago", else the date.
fn ago(secs: i64) -> String {
    match now_secs() - secs {
        ..120 => "Active now".to_owned(),
        gone @ 120..3_600 => format!("{} min ago", gone / 60),
        gone @ 3_600..86_400 => format!("{} h ago", gone / 3_600),
        86_400..172_800 => "Yesterday".to_owned(),
        gone @ 172_800..604_800 => format!("{} days ago", gone / 86_400),
        _ => when(Some(secs)),
    }
}

/// "Locked until 14:05".
fn locked_line(until: i64) -> String {
    let time = rune::time::LocalTime::at(until * 1000);
    format!("Locked until {:02}:{:02}", time.hour, time.minute)
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
    use crate::server::Rule;
    use crate::{SIZE, Screen};

    /// Signed in to the server as account `me` of the sample list, Control Panel open
    /// (administrators on *Users & Groups*, where most of these tests start).
    fn signed_in_as(me: i64) -> Runtime<State> {
        let mut rt = opened_as(me);
        if rt.state.session.as_ref().is_some_and(|s| s.role.is_admin()) {
            rt.state.open_control_tab(Tab::Users);
            while rt.state.inbox.rx.try_recv().is_ok() {}
            rt.state.control.busy = false;
            after_frames(&mut rt);
        }
        rt
    }

    /// Signed in as account `me` of the sample list, Control Panel just opened.
    fn opened_as(me: i64) -> Runtime<State> {
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

    /// Double-click account `id`'s row of the users table, which opens the account.
    fn open_row(rt: &mut Runtime<State>, id: i64) {
        let key = format!("user-{id}");
        let (x, y) = rt
            .find(&key)
            .unwrap_or_else(|| panic!("`{key}` is on screen"));
        for _ in 0..2 {
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
        }
        for _ in 0..4 {
            rt.frame(1.0 / 60.0);
        }
    }

    #[test]
    fn a_super_admin_changes_others_but_not_their_own_role() {
        let mut rt = signed_in_as(1);
        assert!(shown(&rt, "control-tab-users") && shown(&rt, "control-tab-profile"));
        assert!(shown(&rt, "user-4"), "every account is listed");

        open_row(&mut rt, 2);
        for key in ["user-role-superadmin", "user-role-admin", "user-role-user"] {
            assert!(shown(&rt, key), "{key}");
        }
        assert!(shown(&rt, "user-enabled") && shown(&rt, "user-delete"));

        press(&mut rt, "users-back");
        open_row(&mut rt, 1);
        assert!(!shown(&rt, "user-role-user"), "no changing your own role");
        assert!(!shown(&rt, "user-delete"), "no deleting yourself");
    }

    #[test]
    fn an_admin_manages_users_but_not_administrators() {
        let mut rt = signed_in_as(2);
        open_row(&mut rt, 1);
        assert!(!shown(&rt, "user-enabled") && !shown(&rt, "user-delete"));

        press(&mut rt, "users-back");
        open_row(&mut rt, 3);
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
        assert_eq!(state.control.accounts.len(), 4);
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
            ["root", "ada", "eve", "cat", "dan"],
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

    /// Every call sent since the last drain.
    fn sent(state: &State) -> Vec<Reply> {
        std::iter::from_fn(|| state.inbox.rx.try_recv().ok()).collect()
    }

    /// Rebuild the view after changing state directly.
    fn after_frames(rt: &mut Runtime<State>) {
        rt.invalidate();
        for _ in 0..2 {
            rt.frame(1.0 / 60.0);
        }
    }

    #[test]
    fn only_a_super_admin_has_the_groups_tab() {
        assert!(shown(&signed_in_as(1), "control-sub-groups"));
        assert!(!shown(&signed_in_as(2), "control-sub-groups"));
        assert!(!shown(&signed_in_as(3), "control-sub-groups"));
    }

    #[test]
    fn control_panel_opens_on_profile_with_the_classic_sections() {
        let sections = [
            "control-tab-profile",
            "control-tab-appearance",
            "control-tab-users",
            "control-tab-shares",
            "control-tab-storage",
            "control-tab-components",
            "control-tab-settings",
        ];
        let mut rt = opened_as(1);
        assert_eq!(rt.state.control.tab, Tab::Profile);
        assert!(shown(&rt, "profile-current"));
        assert_eq!(page_title(&rt.state), "Profile");
        for key in sections {
            assert!(shown(&rt, key), "{key}");
        }
        assert!(
            !shown(&rt, "control-nav-note"),
            "administrators see everything"
        );

        press(&mut rt, "control-tab-users");
        assert_eq!(page_title(&rt.state), "Users & Groups");
        assert!(shown(&rt, "users-create"), "the header carries the action");
        press(&mut rt, "control-sub-groups");
        assert_eq!(rt.state.control.tab, Tab::Groups);
        assert_eq!(page_title(&rt.state), "Users & Groups");
        assert!(shown(&rt, "groups-create") && !shown(&rt, "users-create"));

        press(&mut rt, "control-tab-storage");
        assert_eq!(page_title(&rt.state), "Storage");
        assert!(shown(&rt, "control-not-yet"), "no made-up numbers");

        let user = opened_as(3);
        assert!(shown(&user, "control-tab-profile") && shown(&user, "control-tab-appearance"));
        for key in &sections[2..] {
            assert!(!shown(&user, key), "{key} is for administrators");
        }
        assert!(shown(&user, "control-nav-note"));
    }

    #[test]
    fn a_narrow_window_turns_the_sidebar_into_an_icon_rail() {
        let width = |rt: &Runtime<State>| rt.presented("control-nav").expect("the sidebar").0[2];
        let mut rt = signed_in_as(1);
        assert!(
            (width(&rt) - 214.0).abs() < 0.5,
            "classic sidebar, got {}",
            width(&rt)
        );
        assert!(shown(&rt, "control-header"));

        // A phone: the window fills the screen, 390 points wide, once its move settles.
        rt.set_viewport((390.0, 844.0), 2.0);
        after_frames(&mut rt);
        for _ in 0..90 {
            rt.frame(1.0 / 60.0);
        }
        assert!(
            (width(&rt) - 56.0).abs() < 0.5,
            "icon rail, got {}",
            width(&rt)
        );
        for key in [
            "control-tab-profile",
            "control-tab-users",
            "control-tab-settings",
        ] {
            assert!(shown(&rt, key), "{key} stays one press away");
        }
        assert!(shown(&rt, "users-create") && shown(&rt, "user-4"));
        assert!(
            !shown(&rt, "users-col-groups") && !shown(&rt, "users-col-seen"),
            "a phone's table leaves out Groups and Last sign-in"
        );
        let you = rt.presented("user-1-you").expect("your badge").0;
        let role = rt.presented("users-col-role").expect("the Role column").0;
        assert!(
            you[0] + you[2] <= role[0],
            "a name too long for the column ends in … and keeps its YOU before Role"
        );
        press(&mut rt, "control-tab-appearance");
        assert_eq!(rt.state.control.tab, Tab::Appearance);
    }

    #[test]
    fn a_super_admin_sets_a_groups_apps_and_members() {
        let mut rt = signed_in_as(1);
        press(&mut rt, "control-sub-groups");
        assert!(
            sent(&rt.state)
                .iter()
                .any(|reply| matches!(reply, Reply::Groups(_))),
            "opening the tab loads the groups"
        );
        crate::with_groups(&mut rt.state);
        after_frames(&mut rt);
        assert!(shown(&rt, "group-1") && shown(&rt, "group-2"));

        press(&mut rt, "group-1");
        for choice in ["inherit", "allow", "deny"] {
            assert!(shown(&rt, &format!("group-app-backup-{choice}")));
        }
        assert!(shown(&rt, "group-member-3") && shown(&rt, "group-delete"));

        press(&mut rt, "group-app-backup-deny");
        assert!(rt.state.control.busy);
        assert!(matches!(sent(&rt.state)[..], [Reply::GroupSaved(_)]));
        press(&mut rt, "group-member-1");
        assert!(sent(&rt.state).is_empty(), "one change at a time");

        let mut staff = rt.state.control.groups[1].clone();
        staff.apps.insert("backup".to_owned(), Rule::Deny);
        rt.state
            .apply_reply(Reply::GroupSaved(Ok(staff)), &mut Tx::default());
        assert!(!rt.state.control.busy);
        assert_eq!(rt.state.control.notice.as_deref(), Some("Saved."));
        assert_eq!(
            rt.state.control.groups[1].apps.get("backup"),
            Some(&Rule::Deny)
        );
    }

    #[test]
    fn deleting_a_group_asks_twice() {
        let mut rt = signed_in_as(1);
        crate::with_groups(&mut rt.state);
        rt.state.open_control_tab(Tab::Groups);
        rt.state.open_group(2);
        rt.state.delete_group(2);
        assert!(sent(&rt.state).is_empty(), "first press asks");
        rt.state.delete_group(2);
        assert!(matches!(sent(&rt.state)[..], [Reply::GroupDeleted(2, _)]));

        rt.state
            .apply_reply(Reply::GroupDeleted(2, Ok(())), &mut Tx::default());
        assert_eq!(rt.state.control.page, Page::List);
        assert_eq!(rt.state.control.groups.len(), 1);
        assert_eq!(
            rt.state.control.notice.as_deref(),
            Some("Deleted the group Interns.")
        );
    }

    /// Account `me`'s Control Panel on cat's *Apps & groups* page.
    fn cats_access_as(me: i64) -> Runtime<State> {
        let mut rt = signed_in_as(me);
        rt.state.open_access(4);
        crate::with_groups(&mut rt.state);
        rt.state.control.access = Some((4, crate::sample_access()));
        after_frames(&mut rt);
        rt
    }

    #[test]
    fn a_super_admin_changes_an_accounts_groups_and_rules() {
        let mut rt = cats_access_as(1);
        assert!(shown(&rt, "access-group-1") && shown(&rt, "access-group-2"));
        assert!(shown(&rt, "access-app-files-deny"));

        press(&mut rt, "access-group-2");
        assert!(matches!(sent(&rt.state)[..], [Reply::UserAccess(4, _)]));
        let mut left = crate::sample_access();
        left.group_ids = vec![1];
        left.groups.retain(|group| group.id == 1);
        rt.state
            .apply_reply(Reply::UserAccess(4, Ok(left)), &mut Tx::default());
        assert_eq!(rt.state.control.notice.as_deref(), Some("Saved."));
        let interns = rt.state.control.groups.iter().find(|g| g.id == 2).unwrap();
        assert!(
            !interns.member_ids.contains(&4),
            "the Groups tab follows the change"
        );
        let staff = rt.state.control.groups.iter().find(|g| g.id == 1).unwrap();
        assert!(staff.member_ids.contains(&4));
    }

    #[test]
    fn an_admin_reads_an_accounts_access_without_changing_it() {
        let rt = cats_access_as(2);
        assert!(shown(&rt, "access-groups") && shown(&rt, "access-apps"));
        assert!(!shown(&rt, "access-group-1"));
        assert!(!shown(&rt, "access-app-files-deny"));
    }

    #[test]
    fn a_user_denied_control_panel_loses_it() {
        let mut rt = signed_in_as(4);
        let mut access = crate::sample_access();
        for decision in &mut access.apps {
            if decision.id == "controlpanel" {
                decision.allowed = false;
            }
        }
        rt.state
            .apply_reply(Reply::MyAccess(Ok(access)), &mut Tx::default());
        assert_eq!(rt.state.desktop.active(), None, "the open window closes");
        rt.state.toggle_desktop_panel(crate::desktop::Panel::Start);
        after_frames(&mut rt);
        assert!(!shown(&rt, "start-control"));

        // A failed refresh keeps what is known; only an ended session signs out.
        let failure = |status| Failure {
            status,
            message: String::new(),
        };
        rt.state
            .apply_reply(Reply::MyAccess(Err(failure(0))), &mut Tx::default());
        assert!(rt.state.signed_in && !rt.state.may_open("controlpanel"));
        rt.state
            .apply_reply(Reply::MyAccess(Err(failure(401))), &mut Tx::default());
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

    /// A maximized window on a tall screen, so long pages fit without scrolling.
    fn roomy(rt: &mut Runtime<State>) {
        rt.state.toggle_app_maximized();
        rt.set_viewport((SIZE.0, 2400.0), 2.0);
        after_frames(rt);
    }

    /// The accounts the users table shows, in order.
    fn listed(rt: &Runtime<State>) -> Vec<i64> {
        rt.state
            .control
            .accounts
            .iter()
            .map(|a| a.id)
            .filter(|id| shown(rt, &format!("user-{id}")))
            .collect()
    }

    #[test]
    fn the_users_table_searches_and_pages() {
        let mut rt = signed_in_as(1);
        roomy(&mut rt);
        assert_eq!(listed(&rt), [1, 2, 3, 4, 5]);
        for key in [
            "users-head",
            "users-col-groups",
            "users-col-seen",
            "user-1-you",
        ] {
            assert!(shown(&rt, key), "{key}");
        }
        assert!(!shown(&rt, "user-2-you"), "You marks only your own row");
        rt.state.control.search = TextEdit::new("staff");
        after_frames(&mut rt);
        assert_eq!(listed(&rt), [3, 4], "groups count");
        rt.state.control.search = TextEdit::new("LOVELACE");
        after_frames(&mut rt);
        assert_eq!(listed(&rt), [2], "display names, ignoring case");
        rt.state.control.search = TextEdit::new("nobody");
        after_frames(&mut rt);
        assert!(listed(&rt).is_empty() && shown(&rt, "users-none"));
        rt.state.control.search = TextEdit::new("");

        // Thirty more accounts: 25 a page, as on classic RAST.
        let bob = rt.state.control.accounts[2].clone();
        for id in 10..40 {
            rt.state.control.accounts.push(Account {
                id,
                username: format!("user{id}"),
                ..bob.clone()
            });
        }
        sort(&mut rt.state.control.accounts);
        after_frames(&mut rt);
        assert_eq!(listed(&rt).len(), 25);
        assert!(shown(&rt, "users-next"));
        press(&mut rt, "users-next");
        assert_eq!(listed(&rt).len(), 10, "the rest on the second page");

        // Typing a search starts again on the first page.
        press(&mut rt, "users-search");
        rt.paste("user1");
        after_frames(&mut rt);
        assert_eq!(rt.state.control.list_page, 0);
        assert_eq!(listed(&rt).len(), 10, "user10 to user19");

        press(&mut rt, "users-per-page");
        assert!(shown(&rt, "users-per-page-menu"));
        press(&mut rt, "users-per-page-10");
        assert_eq!(rt.state.control.per_page, 10);
        assert!(
            !shown(&rt, "users-per-page-menu"),
            "choosing closes the list"
        );
    }

    #[test]
    fn a_rows_menu_disables_after_asking_and_opens_the_account() {
        let mut rt = signed_in_as(1);
        roomy(&mut rt);
        press(&mut rt, "user-3-actions");
        for key in [
            "user-menu",
            "user-menu-edit",
            "user-menu-password",
            "user-menu-sessions",
            "user-menu-disable",
        ] {
            assert!(shown(&rt, key), "{key}");
        }
        press(&mut rt, "user-menu-disable");
        assert_eq!(
            rt.state.control.popup,
            Some(Popup::ConfirmDisable(3)),
            "the first press asks"
        );
        assert!(sent(&rt.state).is_empty(), "nothing sent yet");
        press(&mut rt, "user-menu-disable");
        assert_eq!(rt.state.control.popup, None);
        assert!(matches!(sent(&rt.state)[..], [Reply::Updated(_)]));
        let mut bob = rt.state.control.accounts[2].clone();
        bob.disabled = true;
        rt.state
            .apply_reply(Reply::Updated(Ok(bob)), &mut Tx::default());
        assert_eq!(
            rt.state.control.notice.as_deref(),
            Some("Disabled bob. They were signed out everywhere.")
        );
        after_frames(&mut rt);
        press(&mut rt, "user-3-actions");
        assert!(shown(&rt, "user-menu-enable") && !shown(&rt, "user-menu-disable"));

        press(&mut rt, "user-menu-password");
        assert_eq!(rt.state.control.page, Page::Account(3));
        assert!(!shown(&rt, "user-menu"), "the menu closes with the list");
        after_frames(&mut rt);
        assert!(
            rt.state.control.reset_password.focused,
            "Reset password lands in the temporary password field"
        );
    }

    /// Signed in as the Super Admin on a screen short enough that an account's page scrolls,
    /// the window settled to it. The table's first rows still show whole.
    fn on_a_short_screen() -> Runtime<State> {
        let mut rt = signed_in_as(1);
        rt.set_viewport((SIZE.0, 620.0), 2.0);
        rt.invalidate();
        for _ in 0..90 {
            rt.frame(1.0 / 60.0);
        }
        rt
    }

    /// Whether the element keyed `key` is wholly inside Control Panel's scrolling page, once
    /// it settles.
    fn in_page_view(rt: &mut Runtime<State>, key: &str) -> bool {
        for _ in 0..90 {
            rt.frame(1.0 / 60.0);
        }
        let [_, top, _, tall] = rt.presented("control-content").expect("the page").0;
        let [_, y, _, h] = rt
            .presented(key)
            .unwrap_or_else(|| panic!("`{key}` is laid out"))
            .0;
        y >= top && y + h <= top + tall
    }

    #[test]
    fn view_sessions_scrolls_the_account_to_them() {
        let mut rt = on_a_short_screen();
        press(&mut rt, "user-2-actions");
        press(&mut rt, "user-menu-edit");
        assert!(
            !in_page_view(&mut rt, "user-sessions-title"),
            "Edit user opens at the top, the sessions below the fold"
        );
        press(&mut rt, "users-back");
        press(&mut rt, "user-2-actions");
        press(&mut rt, "user-menu-sessions");
        assert_eq!(rt.state.control.page, Page::Account(2));
        assert!(
            in_page_view(&mut rt, "user-sessions-title"),
            "View sessions scrolls to them"
        );
    }

    #[test]
    fn reset_password_scrolls_to_the_field_and_focuses_it() {
        let mut rt = on_a_short_screen();
        press(&mut rt, "user-2-actions");
        press(&mut rt, "user-menu-edit");
        assert!(
            !in_page_view(&mut rt, "user-new-password"),
            "Edit user opens at the top, the password below the fold"
        );
        press(&mut rt, "users-back");
        press(&mut rt, "user-2-actions");
        press(&mut rt, "user-menu-password");
        assert!(
            in_page_view(&mut rt, "user-new-password"),
            "Reset password scrolls to the field"
        );
        assert!(rt.state.control.reset_password.focused, "and types into it");
    }

    #[test]
    fn each_page_opens_at_its_top() {
        let mut rt = on_a_short_screen();
        press(&mut rt, "user-2-actions");
        press(&mut rt, "user-menu-sessions");
        assert!(
            !in_page_view(&mut rt, "users-back"),
            "the account scrolled down to its sessions"
        );
        rt.state.back_to_accounts();
        after_frames(&mut rt);
        assert!(
            in_page_view(&mut rt, "control-sub"),
            "the list opens at its top"
        );

        // A negative delta moves the content up, toward the list's end.
        let (x, y) = rt.find("users-table").expect("the table");
        rt.wheel(x, y, -2000.0);
        assert!(
            !in_page_view(&mut rt, "control-sub"),
            "the list scrolled to its end"
        );
        open_row(&mut rt, 5);
        assert_eq!(rt.state.control.page, Page::Account(5));
        assert!(
            in_page_view(&mut rt, "users-back"),
            "an account opens at its top"
        );
    }

    #[test]
    fn an_admins_menu_on_a_super_admin_only_opens_it() {
        let mut rt = signed_in_as(2);
        press(&mut rt, "user-1-actions");
        press(&mut rt, "user-menu-disable");
        press(&mut rt, "user-menu-password");
        assert_eq!(
            rt.state.control.popup,
            Some(Popup::Actions(1)),
            "dimmed entries do nothing"
        );
        assert!(sent(&rt.state).is_empty());
        press(&mut rt, "user-menu-edit");
        assert_eq!(rt.state.control.page, Page::Account(1));
    }

    #[test]
    fn the_last_active_super_admin_is_called_out() {
        let mut rt = signed_in_as(1);
        assert!(shown(&rt, "users-one-super-admin"));
        rt.state.control.accounts[1].role = Role::SuperAdmin;
        after_frames(&mut rt);
        assert!(!shown(&rt, "users-one-super-admin"));
    }

    #[test]
    fn a_locked_account_is_unlocked_by_its_administrator() {
        let mut rt = signed_in_as(1);
        open_row(&mut rt, 5);
        assert!(
            matches!(sent(&rt.state)[..], [Reply::UserSessions(5, _, false)]),
            "its sessions load"
        );
        assert!(shown(&rt, "user-unlock"));
        press(&mut rt, "user-unlock");
        assert!(rt.state.control.busy);
        assert!(matches!(sent(&rt.state)[..], [Reply::Updated(_)]));

        let mut dan = rt.state.control.accounts[4].clone();
        dan.locked_until = None;
        dan.failed_attempts = 0;
        rt.state
            .apply_reply(Reply::Updated(Ok(dan)), &mut Tx::default());
        assert_eq!(
            rt.state.control.notice.as_deref(),
            Some("Unlocked. They can sign in again.")
        );
        assert!(
            matches!(sent(&rt.state)[..], [Reply::UserSessions(5, _, false)]),
            "sessions reload after a change"
        );
        after_frames(&mut rt);
        assert!(!shown(&rt, "user-unlock"));

        // A later change says "Saved." again.
        press(&mut rt, "user-must-change");
        rt.state.apply_reply(
            Reply::Updated(Ok(rt.state.control.accounts[4].clone())),
            &mut Tx::default(),
        );
        assert_eq!(
            rt.state.control.notice.as_deref(),
            Some("They choose a new password at their next sign-in, and were signed out.")
        );
        rt.state.save_account_name(5);
        rt.state.apply_reply(
            Reply::Updated(Ok(rt.state.control.accounts[4].clone())),
            &mut Tx::default(),
        );
        assert_eq!(rt.state.control.notice.as_deref(), Some("Saved."));
        after_frames(&mut rt);
        press(&mut rt, "users-back");
        assert_eq!(rt.state.control.notice, None, "the list doesn't repeat it");
    }

    #[test]
    fn administrators_see_sessions_only_of_accounts_they_manage() {
        let mut rt = signed_in_as(2);
        roomy(&mut rt);
        open_row(&mut rt, 1);
        assert!(sent(&rt.state).is_empty(), "no asking about a Super Admin");
        assert!(!shown(&rt, "user-sign-out") && !shown(&rt, "user-unlock"));

        press(&mut rt, "users-back");
        open_row(&mut rt, 3);
        assert!(matches!(
            sent(&rt.state)[..],
            [Reply::UserSessions(3, _, false)]
        ));
        let sessions = crate::sample_sessions();
        rt.state.apply_reply(
            Reply::UserSessions(3, Ok(sessions), false),
            &mut Tx::default(),
        );
        after_frames(&mut rt);
        assert!(shown(&rt, "user-sessions") && shown(&rt, "user-sign-out"));
        press(&mut rt, "user-sign-out");
        assert!(matches!(
            sent(&rt.state)[..],
            [Reply::UserSessions(3, _, true)]
        ));
        rt.state.apply_reply(
            Reply::UserSessions(3, Ok(Vec::new()), true),
            &mut Tx::default(),
        );
        after_frames(&mut rt);
        assert_eq!(
            rt.state.control.notice.as_deref(),
            Some("Signed out everywhere.")
        );
        assert!(!rt.state.control.busy && !shown(&rt, "user-sign-out"));
    }

    #[test]
    fn people_end_their_other_sessions_from_profile() {
        let mut rt = signed_in_as(2);
        roomy(&mut rt);
        press(&mut rt, "control-tab-profile");
        assert!(matches!(sent(&rt.state)[..], [Reply::MySessions(_, false)]));
        rt.state.apply_reply(
            Reply::MySessions(Ok(crate::sample_sessions()), false),
            &mut Tx::default(),
        );
        after_frames(&mut rt);
        assert!(shown(&rt, "session-end-7c21") && shown(&rt, "session-end-b604"));
        assert!(
            !shown(&rt, "session-end-3f9a"),
            "not this one: sign out instead"
        );

        press(&mut rt, "session-end-7c21");
        assert!(matches!(sent(&rt.state)[..], [Reply::MySessions(_, true)]));
        let left: Vec<SessionInfo> = crate::sample_sessions()
            .into_iter()
            .filter(|s| s.id != "7c21")
            .collect();
        rt.state
            .apply_reply(Reply::MySessions(Ok(left), true), &mut Tx::default());
        after_frames(&mut rt);
        assert_eq!(
            rt.state.control.notice.as_deref(),
            Some("Signed out of that session.")
        );
        assert!(!shown(&rt, "session-end-7c21") && shown(&rt, "sessions-end-others"));

        press(&mut rt, "sessions-end-others");
        let only_here: Vec<SessionInfo> = crate::sample_sessions()
            .into_iter()
            .filter(|s| s.current)
            .collect();
        rt.state
            .apply_reply(Reply::MySessions(Ok(only_here), true), &mut Tx::default());
        after_frames(&mut rt);
        assert!(!shown(&rt, "sessions-end-others"));
    }

    #[test]
    fn a_new_display_name_shows_everywhere() {
        let mut rt = signed_in_as(2);
        press(&mut rt, "control-tab-profile");
        assert_eq!(rt.state.control.display_name.text, "Ada Lovelace");
        rt.state.control.display_name = TextEdit::new("Ada King");
        press(&mut rt, "profile-name-save");
        assert!(matches!(
            sent(&rt.state)[..],
            [Reply::MySessions(..), Reply::ProfileSaved(_)]
        ));
        let mut ada = rt.state.control.accounts[1].clone();
        ada.display_name = "Ada King".to_owned();
        rt.state
            .apply_reply(Reply::ProfileSaved(Ok(ada)), &mut Tx::default());
        assert_eq!(rt.state.session.as_ref().unwrap().name(), "Ada King");
        assert_eq!(rt.state.control.accounts[1].name(), "Ada King");
        assert_eq!(rt.state.control.notice.as_deref(), Some("Saved."));
    }

    #[test]
    fn new_accounts_choose_a_password_at_first_sign_in_unless_told_otherwise() {
        let mut rt = signed_in_as(1);
        press(&mut rt, "users-create");
        assert!(rt.state.control.new_must_change);
        press(&mut rt, "new-must-change");
        assert!(!rt.state.control.new_must_change);
        press(&mut rt, "users-back");
        press(&mut rt, "users-create");
        assert!(
            rt.state.control.new_must_change,
            "on again for the next one"
        );
    }
}
