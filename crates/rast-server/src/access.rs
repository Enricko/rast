//! Which apps an account may open: each app's default, rules on groups and
//! rules on the account itself. One evaluator decides for the API gate and
//! explains the same decision in Control Panel.
//!
//! Precedence, as in classic RAST and Synology DSM:
//! 1. Super Admins and Admins open every app.
//! 2. Any deny, on the account or from any of its groups, wins.
//! 3. Otherwise any allow grants the app.
//! 4. Otherwise the app's default applies.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::Role;
use crate::db::{self, User};

/// A stored rule. "Inherit" is the absence of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rule {
    Allow,
    Deny,
}

impl Rule {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// A requested change: `Inherit` removes the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Choice {
    Inherit,
    Allow,
    Deny,
}

impl Choice {
    pub(crate) fn rule(self) -> Option<Rule> {
        match self {
            Self::Inherit => None,
            Self::Allow => Some(Rule::Allow),
            Self::Deny => Some(Rule::Deny),
        }
    }
}

/// An app whose use is controlled. The ids match classic RAST.
pub struct App {
    pub id: &'static str,
    pub label: &'static str,
    /// What a standard user gets when no rule applies.
    pub default_allowed: bool,
}

pub const APPS: [App; 7] = [
    App {
        id: "files",
        label: "File Station",
        default_allowed: true,
    },
    App {
        id: "monitor",
        label: "Resource Monitor",
        default_allowed: true,
    },
    App {
        id: "logs",
        label: "Activity Logs",
        default_allowed: true,
    },
    App {
        id: "controlpanel",
        label: "Control Panel",
        default_allowed: true,
    },
    App {
        id: "applications",
        label: "Docker",
        default_allowed: false,
    },
    App {
        id: "backup",
        label: "Backup Center",
        default_allowed: false,
    },
    App {
        id: "packages",
        label: "App Center",
        default_allowed: false,
    },
];

pub(crate) fn app(id: &str) -> Option<&'static App> {
    APPS.iter().find(|app| app.id == id)
}

/// What decided an app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Administrator,
    /// A rule on the account itself.
    Account,
    Group,
    Default,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub id: &'static str,
    pub label: &'static str,
    pub allowed: bool,
    pub source: Source,
    /// The groups whose rules decided it, when `source` is `Group`.
    pub groups: Vec<String>,
    /// A deny overrode at least one allow.
    pub overridden: bool,
}

/// Decide one app. `own` is the account's rule; `groups` the rules of its
/// groups for this app, as (group name, rule).
pub(crate) fn decide(
    app: &'static App,
    role: Role,
    own: Option<Rule>,
    groups: &[(&str, Rule)],
) -> Decision {
    let decision = |allowed, source, groups: Vec<String>, overridden| Decision {
        id: app.id,
        label: app.label,
        allowed,
        source,
        groups,
        overridden,
    };
    if role.is_admin() {
        return decision(true, Source::Administrator, Vec::new(), false);
    }
    let named = |rule: Rule| -> Vec<String> {
        groups
            .iter()
            .filter(|(_, r)| *r == rule)
            .map(|(name, _)| (*name).to_owned())
            .collect()
    };
    let denying = named(Rule::Deny);
    let allowing = named(Rule::Allow);
    let any_allow = own == Some(Rule::Allow) || !allowing.is_empty();
    if own == Some(Rule::Deny) {
        return decision(false, Source::Account, Vec::new(), any_allow);
    }
    if !denying.is_empty() {
        return decision(false, Source::Group, denying, any_allow);
    }
    if own == Some(Rule::Allow) {
        return decision(true, Source::Account, Vec::new(), false);
    }
    if !allowing.is_empty() {
        return decision(true, Source::Group, allowing, false);
    }
    decision(app.default_allowed, Source::Default, Vec::new(), false)
}

/// Every app's decision for `user`, in catalog order.
pub(crate) async fn effective(
    pool: &SqlitePool,
    user: &User,
) -> Result<Vec<Decision>, sqlx::Error> {
    let own = db::user_rules(pool, user.id).await?;
    let from_groups = db::group_rules_for_user(pool, user.id).await?;
    Ok(APPS
        .iter()
        .map(|app| {
            let groups: Vec<(&str, Rule)> = from_groups
                .iter()
                .filter(|rule| rule.app_id == app.id)
                .map(|rule| (rule.group.as_str(), rule.rule))
                .collect();
            decide(app, user.role, own.get(app.id).copied(), &groups)
        })
        .collect())
}

pub(crate) async fn allowed(
    pool: &SqlitePool,
    user: &User,
    app: &App,
) -> Result<bool, sqlx::Error> {
    Ok(effective(pool, user)
        .await?
        .iter()
        .any(|decision| decision.id == app.id && decision.allowed))
}

/// The app an API path belongs to. Each app's routes live under its prefix,
/// so the gate covers them before the handlers exist.
pub(crate) fn app_for_path(path: &str) -> Option<&'static App> {
    const PREFIXES: [(&str, &str); 6] = [
        ("/api/files", "files"),
        ("/api/monitor", "monitor"),
        ("/api/logs", "logs"),
        ("/api/docker", "applications"),
        ("/api/backups", "backup"),
        ("/api/packages", "packages"),
    ];
    PREFIXES
        .iter()
        .find(|(prefix, _)| {
            path.strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
        .and_then(|(_, id)| app(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker() -> &'static App {
        app("applications").unwrap()
    }

    #[test]
    fn administrators_open_everything() {
        let decision = decide(
            docker(),
            Role::Admin,
            Some(Rule::Deny),
            &[("Interns", Rule::Deny)],
        );
        assert!(decision.allowed);
        assert_eq!(decision.source, Source::Administrator);
    }

    #[test]
    fn a_deny_wins_over_any_allow() {
        let decision = decide(
            docker(),
            Role::User,
            Some(Rule::Allow),
            &[("Staff", Rule::Allow), ("Interns", Rule::Deny)],
        );
        assert!(!decision.allowed);
        assert_eq!(decision.source, Source::Group);
        assert_eq!(decision.groups, ["Interns"]);
        assert!(decision.overridden);

        let own = decide(
            docker(),
            Role::User,
            Some(Rule::Deny),
            &[("Staff", Rule::Allow)],
        );
        assert!(!own.allowed);
        assert_eq!(own.source, Source::Account);
        assert!(own.overridden);
    }

    #[test]
    fn an_allow_beats_the_default_and_names_its_groups() {
        let decision = decide(
            docker(),
            Role::User,
            None,
            &[("Staff", Rule::Allow), ("Ops", Rule::Allow)],
        );
        assert!(decision.allowed);
        assert_eq!(decision.groups, ["Staff", "Ops"]);
        let own = decide(docker(), Role::User, Some(Rule::Allow), &[]);
        assert_eq!((own.allowed, own.source), (true, Source::Account));
    }

    #[test]
    fn without_rules_the_default_applies() {
        assert!(!decide(docker(), Role::User, None, &[]).allowed);
        let files = decide(app("files").unwrap(), Role::User, None, &[]);
        assert_eq!((files.allowed, files.source), (true, Source::Default));
    }

    #[test]
    fn api_paths_map_to_their_app() {
        assert_eq!(app_for_path("/api/files").map(|a| a.id), Some("files"));
        assert_eq!(app_for_path("/api/files/list").map(|a| a.id), Some("files"));
        assert_eq!(
            app_for_path("/api/docker/x").map(|a| a.id),
            Some("applications")
        );
        assert!(app_for_path("/api/filesystem").is_none());
        assert!(app_for_path("/api/users").is_none());
    }
}
