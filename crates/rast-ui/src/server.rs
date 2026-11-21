//! The RAST server's account API, as the desktop uses it.
//!
//! In the browser the page and `/api` share an origin, and the session lives in
//! an HttpOnly cookie the browser sends by itself. Calls run as browser tasks;
//! each reply goes through a channel and wakes the app so `poll` applies it.
//! The native client does not sign in to a server yet, so there every call
//! answers at once that it needs the server.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, channel};

use serde::{Deserialize, Serialize};

/// The three built-in roles (see the server's `Role`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Role {
    #[serde(rename = "superadmin")]
    SuperAdmin,
    #[serde(rename = "admin")]
    Admin,
    #[serde(rename = "user")]
    User,
}

impl Role {
    pub(crate) const ALL: [Role; 3] = [Role::SuperAdmin, Role::Admin, Role::User];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SuperAdmin => "Super Admin",
            Self::Admin => "Admin",
            Self::User => "User",
        }
    }

    pub(crate) fn summary(self) -> &'static str {
        match self {
            Self::SuperAdmin => "Everything, including administrators",
            Self::Admin => "Creates and manages users",
            Self::User => "Uses the desktop",
        }
    }

    pub(crate) fn is_admin(self) -> bool {
        matches!(self, Self::SuperAdmin | Self::Admin)
    }

    /// Mirrors the server's rule: Super Admins manage everyone, Admins users.
    pub(crate) fn may_manage(self, target: Role) -> bool {
        match self {
            Self::SuperAdmin => true,
            Self::Admin => target == Self::User,
            Self::User => false,
        }
    }
}

/// An account as the server lists it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Account {
    pub(crate) id: i64,
    pub(crate) username: String,
    #[serde(default)]
    pub(crate) display_name: String,
    pub(crate) role: Role,
    #[serde(default)]
    pub(crate) disabled: bool,
    #[serde(default)]
    pub(crate) created_at: i64,
    #[serde(default)]
    pub(crate) last_login_at: Option<i64>,
    /// An administrator set the password; a new one is due at the next sign-in.
    #[serde(default)]
    pub(crate) must_change_password: bool,
    /// Sign-in is refused until then after repeated wrong passwords.
    #[serde(default)]
    pub(crate) locked_until: Option<i64>,
    /// Recent wrong passwords.
    #[serde(default)]
    pub(crate) failed_attempts: i64,
    /// Group names.
    #[serde(default)]
    pub(crate) groups: Vec<String>,
}

impl Account {
    /// The display name, or the username when there is none.
    pub(crate) fn name(&self) -> &str {
        if self.display_name.trim().is_empty() {
            &self.username
        } else {
            &self.display_name
        }
    }

    pub(crate) fn locked(&self) -> bool {
        self.locked_until.is_some()
    }
}

/// A device an account is signed in on.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionInfo {
    pub(crate) id: String,
    /// The session this desktop uses.
    #[serde(default)]
    pub(crate) current: bool,
    /// "Chrome on Windows".
    #[serde(default)]
    pub(crate) device: String,
    #[serde(default)]
    pub(crate) ip: String,
    #[serde(default)]
    pub(crate) created_at: i64,
    #[serde(default)]
    pub(crate) last_seen_at: i64,
}

/// A failed call: the HTTP status (0 when the server was unreachable) and a
/// sentence for the user.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Failure {
    pub(crate) status: u16,
    pub(crate) message: String,
}

impl Failure {
    /// The session expired or was ended elsewhere.
    pub(crate) fn signed_out(&self) -> bool {
        self.status == 401
    }
}

// --- groups and app access -------------------------------------------------------

/// A stored app rule; no rule means "inherit".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Rule {
    Allow,
    Deny,
}

/// What Control Panel offers for one app: inherit, allow or deny.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Choice {
    Inherit,
    Allow,
    Deny,
}

impl Choice {
    pub(crate) const ALL: [Choice; 3] = [Choice::Inherit, Choice::Allow, Choice::Deny];

    pub(crate) fn of(rule: Option<Rule>) -> Self {
        match rule {
            None => Self::Inherit,
            Some(Rule::Allow) => Self::Allow,
            Some(Rule::Deny) => Self::Deny,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Inherit => "Inherit",
            Self::Allow => "Allow",
            Self::Deny => "Deny",
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// An app whose use groups and rules control.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppInfo {
    pub(crate) id: String,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) default_allowed: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Group {
    pub(crate) id: i64,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) member_ids: Vec<i64>,
    #[serde(default)]
    pub(crate) apps: BTreeMap<String, Rule>,
}

/// What decided an app for an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Source {
    Administrator,
    Account,
    Group,
    Default,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Decision {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) allowed: bool,
    pub(crate) source: Source,
    #[serde(default)]
    pub(crate) groups: Vec<String>,
    #[serde(default)]
    pub(crate) overridden: bool,
}

impl Decision {
    /// Why, in a few words: "Denied by Interns", "Off by default".
    pub(crate) fn reason(&self) -> String {
        let verdict = if self.allowed { "Allowed" } else { "Denied" };
        let reason = match self.source {
            Source::Administrator => "Administrators open every app".to_owned(),
            Source::Account => format!("{verdict} on this account"),
            Source::Group => format!("{verdict} by {}", self.groups.join(", ")),
            Source::Default if self.allowed => "On by default".to_owned(),
            Source::Default => "Off by default".to_owned(),
        };
        if self.overridden {
            format!("{reason}, which overrides an allow")
        } else {
            reason
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct GroupRef {
    pub(crate) id: i64,
    pub(crate) name: String,
}

/// An account's groups, its own rules and what it may open.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Access {
    #[serde(default)]
    pub(crate) groups: Vec<GroupRef>,
    #[serde(default)]
    pub(crate) group_ids: Vec<i64>,
    #[serde(default)]
    pub(crate) rules: BTreeMap<String, Rule>,
    #[serde(default)]
    pub(crate) apps: Vec<Decision>,
}

impl Access {
    pub(crate) fn allows(&self, app: &str) -> Option<bool> {
        self.apps
            .iter()
            .find(|decision| decision.id == app)
            .map(|decision| decision.allowed)
    }
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GroupChange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) member_ids: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) apps: BTreeMap<String, Choice>,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessChange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) group_ids: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) apps: BTreeMap<String, Choice>,
}

pub(crate) enum Reply {
    Accounts(Result<Vec<Account>, Failure>),
    Created(Result<Account, Failure>),
    Updated(Result<Account, Failure>),
    Deleted(i64, Result<(), Failure>),
    PasswordChanged(Result<(), Failure>),
    Groups(Result<(Vec<Group>, Vec<AppInfo>), Failure>),
    GroupCreated(Result<Group, Failure>),
    GroupSaved(Result<Group, Failure>),
    GroupDeleted(i64, Result<(), Failure>),
    /// An account's access, after loading it or changing it.
    UserAccess(i64, Result<Access, Failure>),
    /// The signed-in account's own access.
    MyAccess(Result<Access, Failure>),
    /// Your sessions, after loading them or (`true`) ending some.
    MySessions(Result<Vec<SessionInfo>, Failure>, bool),
    /// An account's sessions, after loading them or (`true`) ending them.
    UserSessions(i64, Result<Vec<SessionInfo>, Failure>, bool),
    /// Your own display name was saved.
    ProfileSaved(Result<Account, Failure>),
}

/// Where finished calls wait for `poll`.
pub(crate) struct Inbox {
    pub(crate) tx: Sender<Reply>,
    pub(crate) rx: Receiver<Reply>,
}

impl Default for Inbox {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self { tx, rx }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewAccount {
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) password: String,
    pub(crate) role: Role,
    pub(crate) must_change_password: bool,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountChange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) must_change_password: Option<bool>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) unlock: bool,
}

#[derive(Deserialize)]
struct AccountsBody {
    users: Vec<Account>,
}

#[derive(Deserialize)]
struct AccountBody {
    user: Account,
}

#[derive(Deserialize)]
struct SessionsBody {
    sessions: Vec<SessionInfo>,
}

#[derive(Deserialize)]
struct GroupsBody {
    groups: Vec<Group>,
    apps: Vec<AppInfo>,
}

#[derive(Deserialize)]
struct GroupBody {
    group: Group,
}

fn parse<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T, Failure> {
    serde_json::from_str(body).map_err(|_| Failure {
        status: 0,
        message: "The RAST server sent an answer this desktop doesn't understand. Reload the page."
            .to_owned(),
    })
}

/// Starts calls; each finishes later as a `Reply` in the inbox.
pub(crate) struct Calls<'a> {
    pub(crate) inbox: &'a Inbox,
    pub(crate) wake: Option<rune::Waker>,
}

impl Calls<'_> {
    pub(crate) fn accounts(&self) {
        self.run(
            "GET",
            "/api/users".to_owned(),
            None,
            |result: Result<String, Failure>| {
                Reply::Accounts(
                    result.and_then(|body| parse::<AccountsBody>(&body).map(|b| b.users)),
                )
            },
        );
    }

    pub(crate) fn create(&self, account: &NewAccount) {
        let body = serde_json::to_string(account).ok();
        self.run("POST", "/api/users".to_owned(), body, |result| {
            Reply::Created(result.and_then(|body| parse::<AccountBody>(&body).map(|b| b.user)))
        });
    }

    pub(crate) fn update(&self, id: i64, change: &AccountChange) {
        let body = serde_json::to_string(change).ok();
        self.run("PATCH", format!("/api/users/{id}"), body, |result| {
            Reply::Updated(result.and_then(|body| parse::<AccountBody>(&body).map(|b| b.user)))
        });
    }

    pub(crate) fn delete(&self, id: i64) {
        self.run("DELETE", format!("/api/users/{id}"), None, move |result| {
            Reply::Deleted(id, result.map(|_| ()))
        });
    }

    pub(crate) fn change_password(&self, current: &str, new: &str) {
        let body = serde_json::json!({ "current": current, "new": new }).to_string();
        self.run(
            "POST",
            "/api/auth/password".to_owned(),
            Some(body),
            |result| Reply::PasswordChanged(result.map(|_| ())),
        );
    }

    pub(crate) fn save_profile(&self, display_name: &str) {
        let body = serde_json::json!({ "displayName": display_name }).to_string();
        self.run(
            "PATCH",
            "/api/auth/profile".to_owned(),
            Some(body),
            |result| {
                Reply::ProfileSaved(
                    result.and_then(|body| parse::<AccountBody>(&body).map(|b| b.user)),
                )
            },
        );
    }

    fn sessions_call(&self, method: &'static str, path: String) {
        let changed = method != "GET";
        self.run(method, path, None, move |result| {
            Reply::MySessions(
                result.and_then(|body| parse::<SessionsBody>(&body).map(|b| b.sessions)),
                changed,
            )
        });
    }

    pub(crate) fn my_sessions(&self) {
        self.sessions_call("GET", "/api/auth/sessions".to_owned());
    }

    pub(crate) fn end_my_session(&self, id: &str) {
        self.sessions_call("DELETE", format!("/api/auth/sessions/{id}"));
    }

    pub(crate) fn end_my_other_sessions(&self) {
        self.sessions_call("DELETE", "/api/auth/sessions".to_owned());
    }

    fn user_sessions_call(&self, method: &'static str, id: i64) {
        let changed = method != "GET";
        self.run(
            method,
            format!("/api/users/{id}/sessions"),
            None,
            move |result| {
                Reply::UserSessions(
                    id,
                    result.and_then(|body| parse::<SessionsBody>(&body).map(|b| b.sessions)),
                    changed,
                )
            },
        );
    }

    pub(crate) fn user_sessions(&self, id: i64) {
        self.user_sessions_call("GET", id);
    }

    pub(crate) fn end_user_sessions(&self, id: i64) {
        self.user_sessions_call("DELETE", id);
    }

    pub(crate) fn groups(&self) {
        self.run("GET", "/api/groups".to_owned(), None, |result| {
            Reply::Groups(
                result.and_then(|body| parse::<GroupsBody>(&body).map(|b| (b.groups, b.apps))),
            )
        });
    }

    pub(crate) fn create_group(&self, name: &str, description: &str) {
        let body = serde_json::json!({ "name": name, "description": description }).to_string();
        self.run("POST", "/api/groups".to_owned(), Some(body), |result| {
            Reply::GroupCreated(result.and_then(|body| parse::<GroupBody>(&body).map(|b| b.group)))
        });
    }

    pub(crate) fn update_group(&self, id: i64, change: &GroupChange) {
        let body = serde_json::to_string(change).ok();
        self.run("PATCH", format!("/api/groups/{id}"), body, |result| {
            Reply::GroupSaved(result.and_then(|body| parse::<GroupBody>(&body).map(|b| b.group)))
        });
    }

    pub(crate) fn delete_group(&self, id: i64) {
        self.run("DELETE", format!("/api/groups/{id}"), None, move |result| {
            Reply::GroupDeleted(id, result.map(|_| ()))
        });
    }

    pub(crate) fn user_access(&self, id: i64) {
        self.run(
            "GET",
            format!("/api/users/{id}/access"),
            None,
            move |result| Reply::UserAccess(id, result.and_then(|body| parse::<Access>(&body))),
        );
    }

    pub(crate) fn update_user_access(&self, id: i64, change: &AccessChange) {
        let body = serde_json::to_string(change).ok();
        self.run(
            "PATCH",
            format!("/api/users/{id}/access"),
            body,
            move |result| Reply::UserAccess(id, result.and_then(|body| parse::<Access>(&body))),
        );
    }

    pub(crate) fn my_access(&self) {
        self.run("GET", "/api/access/me".to_owned(), None, |result| {
            Reply::MyAccess(result.and_then(|body| parse::<Access>(&body)))
        });
    }

    #[cfg(target_arch = "wasm32")]
    fn run(
        &self,
        method: &'static str,
        path: String,
        body: Option<String>,
        finish: impl FnOnce(Result<String, Failure>) -> Reply + 'static,
    ) {
        let tx = self.inbox.tx.clone();
        let wake = self.wake.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let reply = finish(web::request(method, &path, body).await);
            let _ = tx.send(reply);
            if let Some(wake) = wake {
                wake();
            }
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn run(
        &self,
        _method: &'static str,
        _path: String,
        _body: Option<String>,
        finish: impl FnOnce(Result<String, Failure>) -> Reply + 'static,
    ) {
        let _ = self.inbox.tx.send(finish(Err(Failure {
            status: 0,
            message: "Accounts are managed in the browser, signed in to the RAST server."
                .to_owned(),
        })));
        if let Some(wake) = &self.wake {
            wake();
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;

    use super::Failure;

    fn unreachable() -> Failure {
        Failure {
            status: 0,
            message: "The RAST server can't be reached. Check your connection and try again."
                .to_owned(),
        }
    }

    /// One same-origin call; the browser adds the session cookie.
    pub(super) async fn request(
        method: &str,
        path: &str,
        body: Option<String>,
    ) -> Result<String, Failure> {
        let init = web_sys::RequestInit::new();
        init.set_method(method);
        let headers = web_sys::Headers::new().map_err(|_| unreachable())?;
        headers.set("x-rast", "1").map_err(|_| unreachable())?;
        if let Some(body) = &body {
            headers
                .set("content-type", "application/json")
                .map_err(|_| unreachable())?;
            init.set_body(&JsValue::from_str(body));
        }
        init.set_headers(&headers);
        let request =
            web_sys::Request::new_with_str_and_init(path, &init).map_err(|_| unreachable())?;
        let window = web_sys::window().ok_or_else(unreachable)?;
        let response: web_sys::Response = JsFuture::from(window.fetch_with_request(&request))
            .await
            .map_err(|_| unreachable())?
            .dyn_into()
            .map_err(|_| unreachable())?;
        let status = response.status();
        let text = match response.text() {
            Ok(promise) => JsFuture::from(promise)
                .await
                .ok()
                .and_then(|value| value.as_string())
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
        if response.ok() {
            return Ok(text);
        }
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| match status {
                502..=504 => "The RAST server is not responding. Try again in a moment.".to_owned(),
                _ => format!("The RAST server answered {status}."),
            });
        Err(Failure { status, message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_parse_from_the_server_shape() {
        let body = r#"{"users":[{"id":1,"username":"root","displayName":"","role":"superadmin","disabled":false,"createdAt":1,"lastLoginAt":null},
                                 {"id":2,"username":"bob","displayName":"Bob B","role":"user","disabled":true,"createdAt":2,"lastLoginAt":5}]}"#;
        let users = parse::<AccountsBody>(body).unwrap().users;
        assert_eq!(users[0].role, Role::SuperAdmin);
        assert_eq!(users[0].name(), "root");
        assert_eq!(users[1].name(), "Bob B");
        assert!(users[1].disabled);
    }

    #[test]
    fn protection_fields_and_sessions_parse() {
        let body = r#"{"users":[{"id":3,"username":"cat","role":"user","mustChangePassword":true,
            "lockedUntil":1800000000,"failedAttempts":10,"groups":["Staff"]}]}"#;
        let cat = &parse::<AccountsBody>(body).unwrap().users[0];
        assert!(cat.must_change_password && cat.locked());
        assert_eq!(cat.groups, ["Staff"]);
        let older = r#"{"users":[{"id":1,"username":"root","role":"superadmin"}]}"#;
        let root = &parse::<AccountsBody>(older).unwrap().users[0];
        assert!(
            !root.locked() && root.groups.is_empty(),
            "fields are optional"
        );

        let sessions = r#"{"sessions":[{"id":"ab","current":true,"device":"Chrome on Linux",
            "ip":"203.0.113.5","createdAt":1,"lastSeenAt":2,"expiresAt":3},
            {"id":"cd","current":false,"device":"Safari on iOS","ip":"","createdAt":1,"lastSeenAt":1}]}"#;
        let sessions = parse::<SessionsBody>(sessions).unwrap().sessions;
        assert!(sessions[0].current && !sessions[1].current);
        assert_eq!(
            (sessions[0].device.as_str(), sessions[0].ip.as_str()),
            ("Chrome on Linux", "203.0.113.5")
        );
        assert_eq!(
            (sessions[1].id.as_str(), sessions[1].last_seen_at),
            ("cd", 1)
        );
    }

    #[test]
    fn unlocking_and_forced_changes_serialize() {
        let change = AccountChange {
            unlock: true,
            ..AccountChange::default()
        };
        assert_eq!(
            serde_json::to_string(&change).unwrap(),
            r#"{"unlock":true}"#
        );
        let reset = AccountChange {
            password: Some("temporary pass".to_owned()),
            must_change_password: Some(true),
            ..AccountChange::default()
        };
        assert_eq!(
            serde_json::to_string(&reset).unwrap(),
            r#"{"password":"temporary pass","mustChangePassword":true}"#
        );
    }

    #[test]
    fn changes_send_only_what_changed() {
        let change = AccountChange {
            disabled: Some(true),
            ..AccountChange::default()
        };
        assert_eq!(
            serde_json::to_string(&change).unwrap(),
            r#"{"disabled":true}"#
        );
        let role = AccountChange {
            role: Some(Role::Admin),
            ..AccountChange::default()
        };
        assert_eq!(serde_json::to_string(&role).unwrap(), r#"{"role":"admin"}"#);
    }

    #[test]
    fn access_parses_and_explains_itself() {
        let body = r#"{"groups":[{"id":1,"name":"Interns"}],"groupIds":[1],"rules":{"files":"allow"},
            "apps":[{"id":"applications","label":"Docker","allowed":false,"source":"group","groups":["Interns"],"overridden":true},
                    {"id":"files","label":"File Station","allowed":true,"source":"account","groups":[],"overridden":false},
                    {"id":"backup","label":"Backup Center","allowed":false,"source":"default"}]}"#;
        let access = parse::<Access>(body).unwrap();
        assert_eq!(access.rules["files"], Rule::Allow);
        assert_eq!(access.allows("applications"), Some(false));
        assert_eq!(access.allows("nope"), None);
        assert_eq!(
            access.apps[0].reason(),
            "Denied by Interns, which overrides an allow"
        );
        assert_eq!(access.apps[1].reason(), "Allowed on this account");
        assert_eq!(access.apps[2].reason(), "Off by default");
    }

    #[test]
    fn group_changes_send_inherit_to_clear_a_rule() {
        let mut change = GroupChange::default();
        change.apps.insert("files".to_owned(), Choice::Inherit);
        assert_eq!(
            serde_json::to_string(&change).unwrap(),
            r#"{"apps":{"files":"inherit"}}"#
        );
        let members = GroupChange {
            member_ids: Some(vec![2, 3]),
            ..GroupChange::default()
        };
        assert_eq!(
            serde_json::to_string(&members).unwrap(),
            r#"{"memberIds":[2,3]}"#
        );
    }

    #[test]
    fn roles_mirror_the_server_rules() {
        assert!(Role::SuperAdmin.may_manage(Role::Admin));
        assert!(Role::Admin.may_manage(Role::User));
        assert!(!Role::Admin.may_manage(Role::Admin));
        assert!(!Role::User.may_manage(Role::User));
    }
}
