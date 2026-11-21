//! The RAST server's account API, as the desktop uses it.
//!
//! In the browser the page and `/api` share an origin, and the session lives in
//! an HttpOnly cookie the browser sends by itself. Calls run as browser tasks;
//! each reply goes through a channel and wakes the app so `poll` applies it.
//! The native client does not sign in to a server yet, so there every call
//! answers at once that it needs the server.

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

pub(crate) enum Reply {
    Accounts(Result<Vec<Account>, Failure>),
    Created(Result<Account, Failure>),
    Updated(Result<Account, Failure>),
    Deleted(i64, Result<(), Failure>),
    PasswordChanged(Result<(), Failure>),
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
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountChange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) password: Option<String>,
}

#[derive(Deserialize)]
struct AccountsBody {
    users: Vec<Account>,
}

#[derive(Deserialize)]
struct AccountBody {
    user: Account,
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
    fn roles_mirror_the_server_rules() {
        assert!(Role::SuperAdmin.may_manage(Role::Admin));
        assert!(Role::Admin.may_manage(Role::User));
        assert!(!Role::Admin.may_manage(Role::Admin));
        assert!(!Role::User.may_manage(Role::User));
    }
}
