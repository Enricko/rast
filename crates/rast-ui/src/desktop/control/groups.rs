//! Control Panel → Groups, and an account's *Apps & groups* page.
//!
//! A group allows or denies apps for its members. On an account, rules of its
//! own sit next to its groups' rules. A deny from anywhere wins, then any
//! allow, then the app's default. Administrators open every app. The server
//! decides; these pages show its answer and why.

use rune::{El, TextEdit, col, row, text};

use super::{
    BODY, DETAIL, Look, NAME, Page, Palette, State, TITLE, Tab, app_row, avatar, back_link, button,
    choice_bar, divider, field, header_message, messages, section, switch, tile,
};
use crate::server::{Access, AccessChange, Account, Choice, Group, GroupChange, Role};

// --- actions ------------------------------------------------------------------------

impl State {
    pub(crate) fn open_group(&mut self, id: i64) {
        let control = &mut self.control;
        let (name, description) = control
            .groups
            .iter()
            .find(|g| g.id == id)
            .map(|g| (g.name.clone(), g.description.clone()))
            .unwrap_or_default();
        control.page = Page::Group(id);
        control.group_name = TextEdit::new(name);
        control.group_description = TextEdit::new(description);
        control.confirm_delete = false;
        control.error = None;
        control.notice = None;
    }

    /// Back to the list. What was said about the page you leave goes with it.
    pub(crate) fn back_to_groups(&mut self) {
        self.control.page = Page::List;
        self.control.error = None;
        self.control.notice = None;
        self.control.confirm_delete = false;
    }

    pub(crate) fn start_create_group(&mut self) {
        let control = &mut self.control;
        control.page = Page::CreateGroup;
        control.group_name = TextEdit::new("");
        control.group_description = TextEdit::new("");
        control.error = None;
        control.notice = None;
    }

    pub(crate) fn submit_create_group(&mut self) {
        if self.control.busy {
            return;
        }
        let name = self.control.group_name.text.trim().to_owned();
        if name.is_empty() {
            self.control.error = Some("Give the group a name.".to_owned());
            return;
        }
        let description = self.control.group_description.text.trim().to_owned();
        self.control.busy = true;
        self.control.error = None;
        self.calls().create_group(&name, &description);
    }

    fn change_group(&mut self, id: i64, change: GroupChange) {
        if self.control.busy {
            return;
        }
        self.control.busy = true;
        self.control.error = None;
        self.control.notice = None;
        self.calls().update_group(id, &change);
    }

    pub(crate) fn save_group_details(&mut self, id: i64) {
        let name = self.control.group_name.text.trim().to_owned();
        if name.is_empty() {
            self.control.error = Some("Give the group a name.".to_owned());
            return;
        }
        let description = self.control.group_description.text.trim().to_owned();
        self.change_group(
            id,
            GroupChange {
                name: Some(name),
                description: Some(description),
                ..GroupChange::default()
            },
        );
    }

    pub(crate) fn toggle_group_member(&mut self, id: i64, member: i64) {
        let Some(group) = self.control.groups.iter().find(|g| g.id == id) else {
            return;
        };
        let mut members = group.member_ids.clone();
        if let Some(at) = members.iter().position(|m| *m == member) {
            members.remove(at);
        } else {
            members.push(member);
        }
        self.change_group(
            id,
            GroupChange {
                member_ids: Some(members),
                ..GroupChange::default()
            },
        );
    }

    pub(crate) fn set_group_rule(&mut self, id: i64, app: &str, choice: Choice) {
        let mut change = GroupChange::default();
        change.apps.insert(app.to_owned(), choice);
        self.change_group(id, change);
    }

    /// The first press asks to confirm; the second deletes.
    pub(crate) fn delete_group(&mut self, id: i64) {
        if self.control.busy {
            return;
        }
        if !self.control.confirm_delete {
            self.control.confirm_delete = true;
            return;
        }
        self.control.busy = true;
        self.control.error = None;
        self.calls().delete_group(id);
    }

    pub(super) fn group_created(&mut self, group: Group) {
        let (id, name) = (group.id, group.name.clone());
        self.control.groups.push(group);
        sort_groups(&mut self.control.groups);
        self.open_group(id);
        self.control.notice = Some(format!("Created {name}. Now choose its members and apps."));
    }

    pub(super) fn group_saved(&mut self, group: Group) {
        match self.control.groups.iter_mut().find(|g| g.id == group.id) {
            Some(slot) => *slot = group,
            None => self.control.groups.push(group),
        }
        sort_groups(&mut self.control.groups);
        self.control.notice = Some("Saved.".to_owned());
    }

    pub(super) fn group_deleted(&mut self, id: i64) {
        if let Some(gone) = self.control.groups.iter().find(|g| g.id == id) {
            self.control.notice = Some(format!("Deleted the group {}.", gone.name));
        }
        self.control.groups.retain(|g| g.id != id);
        self.control.page = Page::List;
        self.control.confirm_delete = false;
    }

    /// An account's *Apps & groups* page: load its access and the groups.
    pub(crate) fn open_access(&mut self, id: i64) {
        self.control.page = Page::Access(id);
        self.control.access = None;
        self.control.error = None;
        self.control.notice = None;
        self.calls().user_access(id);
        self.load_groups();
    }

    fn change_access(&mut self, id: i64, change: AccessChange) {
        if self.control.busy {
            return;
        }
        self.control.busy = true;
        self.control.error = None;
        self.control.notice = None;
        self.calls().update_user_access(id, &change);
    }

    pub(crate) fn toggle_access_group(&mut self, id: i64, group: i64) {
        let Some((_, access)) = self.control.access.as_ref().filter(|(uid, _)| *uid == id) else {
            return;
        };
        let mut groups = access.group_ids.clone();
        if let Some(at) = groups.iter().position(|g| *g == group) {
            groups.remove(at);
        } else {
            groups.push(group);
        }
        self.change_access(
            id,
            AccessChange {
                group_ids: Some(groups),
                ..AccessChange::default()
            },
        );
    }

    pub(crate) fn set_access_rule(&mut self, id: i64, app: &str, choice: Choice) {
        let mut change = AccessChange::default();
        change.apps.insert(app.to_owned(), choice);
        self.change_access(id, change);
    }
}

fn sort_groups(groups: &mut [Group]) {
    groups.sort_by_key(|g| g.name.to_lowercase());
}

fn members_line(count: usize) -> String {
    match count {
        0 => "No members".to_owned(),
        1 => "1 member".to_owned(),
        n => format!("{n} members"),
    }
}

fn rules_line(count: usize) -> String {
    match count {
        0 => "no app rules".to_owned(),
        1 => "1 app rule".to_owned(),
        n => format!("{n} app rules"),
    }
}

fn capitalized(line: &str) -> String {
    let mut chars = line.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

// --- Groups tab ---------------------------------------------------------------------

pub(super) fn page(state: &State, p: &Palette, compact: bool) -> El {
    let control = &state.control;
    match control.page {
        Page::CreateGroup => create_page(state, p),
        Page::Group(id) => match control.groups.iter().find(|g| g.id == id) {
            Some(group) => group_page(state, p, group, compact),
            None => list(state, p),
        },
        _ => list(state, p),
    }
}

fn list(state: &State, p: &Palette) -> El {
    let control = &state.control;
    let count = if !control.groups_loaded {
        "Loading groups…".to_owned()
    } else {
        match control.groups.len() {
            1 => "1 group".to_owned(),
            n => format!("{n} groups"),
        }
    };
    let mut page = col()
        .w_full()
        .gap(12.0)
        .child(super::shell::users_and_groups(p, Tab::Groups))
        .child(super::shell::intro(
            p,
            Some("Groups"),
            "Members get the apps a group allows. A deny, from any group or on the \
             account, always wins; Super Admins and Admins open every app.",
        ));
    page = messages(page, state, p);
    page = page.child(text(count).key("groups-count").font(DETAIL).color(p.muted));
    if control.groups_loaded && control.groups.is_empty() {
        return page.child(
            text("No groups yet. Create one to allow or deny apps for several people at once.")
                .key("groups-empty")
                .font(BODY)
                .color(p.muted)
                .w_full()
                .wrap(),
        );
    }
    let mut rows = col().key("groups-list").w_full().gap(2.0);
    for group in &control.groups {
        let id = group.id;
        rows = rows.child(
            row()
                .key(format!("group-{id}"))
                .role("button")
                .label(group.name.clone())
                .w_full()
                .h(50.0)
                .px(10.0)
                .gap(10.0)
                .center_y()
                .radius(8.0)
                .hover(p.hover)
                .child(tile(state, p, "groups", 32.0))
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(2.0)
                        .child(text(group.name.clone()).font(NAME).color(p.ink).truncate())
                        .child(
                            text(format!(
                                "{} · {}",
                                members_line(group.member_ids.len()),
                                rules_line(group.apps.len())
                            ))
                            .font(DETAIL)
                            .color(p.muted)
                            .truncate(),
                        ),
                )
                .child(text("›").font(BODY).color(p.muted).a11y_hidden())
                .on_press(move |s: &mut State| s.open_group(id)),
        );
    }
    page.child(rows)
}

fn create_page(state: &State, p: &Palette) -> El {
    let control = &state.control;
    let page = col()
        .key("create-group")
        .w_full()
        .max_w(440.0)
        .gap(12.0)
        .child(back_link(p, "groups-back", "All groups").on_press(State::back_to_groups))
        .child(text("Create group").font(TITLE).color(p.ink))
        .child(field(
            p,
            &control.group_name,
            "new-group-name",
            "Name",
            "e.g. Staff",
            |s| &mut s.control.group_name,
        ))
        .child(field(
            p,
            &control.group_description,
            "new-group-description",
            "Description (optional)",
            "Who is in it",
            |s| &mut s.control.group_description,
        ));
    messages(page, state, p).child(
        row()
            .gap(8.0)
            .pt(4.0)
            .child(
                button(
                    p,
                    "create-group-submit",
                    if control.busy {
                        "Creating…"
                    } else {
                        "Create group"
                    },
                    Look::Primary,
                )
                .on_press(State::submit_create_group),
            )
            .child(
                button(p, "create-group-cancel", "Cancel", Look::Plain)
                    .on_press(State::back_to_groups),
            ),
    )
}

fn group_page(state: &State, p: &Palette, group: &Group, compact: bool) -> El {
    let control = &state.control;
    let id = group.id;
    let header = row()
        .w_full()
        .gap(12.0)
        .center_y()
        .child(tile(state, p, "groups", 44.0))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(text(group.name.clone()).font(TITLE).color(p.ink).truncate())
                .child(
                    text(format!(
                        "{} · {}",
                        members_line(group.member_ids.len()),
                        rules_line(group.apps.len())
                    ))
                    .font(DETAIL)
                    .color(p.muted)
                    .truncate(),
                ),
        );
    let mut page = col()
        .key("group-page")
        .w_full()
        .gap(12.0)
        .child(back_link(p, "groups-back", "All groups").on_press(State::back_to_groups))
        .child(header_message(header, state, p, compact));

    page = page.child(section(p, "APPLICATIONS")).child(
        text("Inherit leaves the app to the member's other groups, their own rules and the app's default.")
            .font(DETAIL)
            .color(p.muted)
            .w_full()
            .wrap(),
    );
    let mut apps = col().key("group-apps").w_full().gap(10.0);
    for app in &control.apps {
        let app_id = app.id.clone();
        let bar = choice_bar(
            p,
            format!("group-app-{}", app.id),
            &app.label,
            Choice::of(group.apps.get(&app.id).copied()),
            move |choice| {
                let app_id = app_id.clone();
                move |s: &mut State| s.set_group_rule(id, &app_id, choice)
            },
        );
        let line = if app.default_allowed {
            "Users have it by default"
        } else {
            "Off for Users by default"
        };
        apps = apps.child(stacked(
            compact,
            app_row(p, &app.label, line.to_owned(), None, None),
            bar,
        ));
    }
    page = page.child(apps);

    page = page.child(section(p, "MEMBERS"));
    let mut members = col().key("group-members").w_full().gap(4.0);
    for account in &control.accounts {
        let member = account.id;
        let on = group.member_ids.contains(&member);
        let mut line = format!("@{} · {}", account.username, account.role.label());
        if account.role.is_admin() {
            line.push_str(" · opens every app");
        }
        members = members.child(
            row()
                .w_full()
                .h(44.0)
                .gap(10.0)
                .center_y()
                .child(avatar(p, account, 28.0))
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(1.0)
                        .child(
                            text(account.name().to_owned())
                                .font(NAME)
                                .color(p.ink)
                                .truncate(),
                        )
                        .child(text(line).font(DETAIL).color(p.muted).truncate()),
                )
                .child(
                    switch(
                        p,
                        format!("group-member-{member}"),
                        format!("{} is a member", account.name()),
                        on,
                    )
                    .on_press(move |s: &mut State| s.toggle_group_member(id, member)),
                ),
        );
    }
    page = page.child(members);

    page = page.child(section(p, "DETAILS")).child(
        col()
            .w_full()
            .max_w(440.0)
            .gap(10.0)
            .child(field(
                p,
                &control.group_name,
                "group-name",
                "Name",
                "",
                |s| &mut s.control.group_name,
            ))
            .child(field(
                p,
                &control.group_description,
                "group-description",
                "Description",
                "Who is in it",
                |s| &mut s.control.group_description,
            ))
            .child(
                row().child(
                    button(p, "group-save", "Save", Look::Plain)
                        .on_press(move |s: &mut State| s.save_group_details(id)),
                ),
            ),
    );

    let label = if control.confirm_delete {
        format!("Press again to delete {}", group.name)
    } else {
        "Delete group".to_owned()
    };
    page.child(divider(p))
        .child(
            text("Deleting a group keeps its members' accounts; they only lose the group's rules.")
                .font(DETAIL)
                .color(p.muted)
                .w_full()
                .wrap(),
        )
        .child(
            row().child(
                button(p, "group-delete", &label, Look::Danger)
                    .on_press(move |s: &mut State| s.delete_group(id)),
            ),
        )
}

/// Side by side on wide windows; the control under the text on narrow ones.
fn stacked(compact: bool, info: El, control: El) -> El {
    if compact {
        col().w_full().gap(6.0).child(info).child(control)
    } else {
        row()
            .w_full()
            .gap(10.0)
            .center_y()
            .child(info.grow(1.0).min_w(0.0))
            .child(control)
    }
}

// --- an account's apps and groups ----------------------------------------------------

pub(super) fn access_page(
    state: &State,
    p: &Palette,
    me: &Account,
    account: &Account,
    compact: bool,
) -> El {
    let control = &state.control;
    let id = account.id;
    let editable = me.role == Role::SuperAdmin;
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
                .child(text("Apps & groups").font(TITLE).color(p.ink).truncate())
                .child(
                    text(format!("@{} · {}", account.username, account.role.label()))
                        .font(DETAIL)
                        .color(p.muted)
                        .truncate(),
                ),
        );
    let mut page = col()
        .key("access-page")
        .w_full()
        .gap(12.0)
        .child(
            back_link(p, "access-back", account.name())
                .on_press(move |s: &mut State| s.open_account(id)),
        )
        .child(header_message(header, state, p, compact));
    let Some(access) = control
        .access
        .as_ref()
        .filter(|(uid, _)| *uid == id)
        .map(|(_, access)| access)
    else {
        return page.child(text("Loading…").font(DETAIL).color(p.muted));
    };
    if account.role.is_admin() {
        page = page.child(
            text("Administrators open every app. Groups and rules here apply if the role changes to User.")
                .font(DETAIL)
                .color(p.muted)
                .w_full()
                .wrap(),
        );
    }
    page = page
        .child(section(p, "GROUPS"))
        .child(groups_of(state, p, access, id, editable));

    page = page.child(section(p, "APPLICATIONS")).child(
        text(if editable {
            "Each app shows what this account can open and why. A rule here sits next to its groups' rules; a deny anywhere wins."
        } else {
            "What this account can open and why. Only a Super Admin changes groups and rules."
        })
        .font(DETAIL)
        .color(p.muted)
        .w_full()
        .wrap(),
    );
    let mut apps = col().key("access-apps").w_full().gap(10.0);
    for decision in &access.apps {
        let info = app_row(
            p,
            &decision.label,
            decision.reason(),
            Some(decision.allowed),
            None,
        );
        apps = apps.child(if editable {
            let app_id = decision.id.clone();
            let bar = choice_bar(
                p,
                format!("access-app-{}", decision.id),
                &decision.label,
                Choice::of(access.rules.get(&decision.id).copied()),
                move |choice| {
                    let app_id = app_id.clone();
                    move |s: &mut State| s.set_access_rule(id, &app_id, choice)
                },
            );
            stacked(compact, info, bar)
        } else {
            info
        });
    }
    page.child(apps)
}

fn groups_of(state: &State, p: &Palette, access: &Access, id: i64, editable: bool) -> El {
    let control = &state.control;
    if !editable || control.groups.is_empty() {
        let names: Vec<&str> = access.groups.iter().map(|g| g.name.as_str()).collect();
        let line = match (
            names.is_empty(),
            control.groups_loaded && control.groups.is_empty(),
        ) {
            (true, true) if editable => "No groups yet. Create them under Groups.".to_owned(),
            (true, _) => "Not in any group.".to_owned(),
            (false, _) => names.join(", "),
        };
        return text(line)
            .key("access-groups")
            .font(BODY)
            .color(p.ink)
            .w_full()
            .wrap();
    }
    let mut list = col().key("access-groups").w_full().gap(4.0);
    for group in &control.groups {
        let gid = group.id;
        let on = access.group_ids.contains(&gid);
        list = list.child(
            row()
                .w_full()
                .h(40.0)
                .gap(10.0)
                .center_y()
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(1.0)
                        .child(text(group.name.clone()).font(NAME).color(p.ink).truncate())
                        .child(
                            text(capitalized(&rules_line(group.apps.len())))
                                .font(DETAIL)
                                .color(p.muted),
                        ),
                )
                .child(
                    switch(
                        p,
                        format!("access-group-{gid}"),
                        format!("Member of {}", group.name),
                        on,
                    )
                    .on_press(move |s: &mut State| s.toggle_access_group(id, gid)),
                ),
        );
    }
    list
}
