//! The three built-in roles and who may manage whom.

use serde::{Deserialize, Serialize};

/// Built-in roles. Groups (a later step) grant or deny apps on top of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Everything, including creating and managing administrators.
    #[serde(rename = "superadmin")]
    SuperAdmin,
    /// Creates and manages standard users.
    Admin,
    /// Uses the desktop; manages only their own password.
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SuperAdmin => "superadmin",
            Self::Admin => "admin",
            Self::User => "user",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "superadmin" => Some(Self::SuperAdmin),
            "admin" => Some(Self::Admin),
            "user" => Some(Self::User),
            _ => None,
        }
    }

    /// Lists show Super Admins first.
    pub fn rank(self) -> u8 {
        match self {
            Self::SuperAdmin => 0,
            Self::Admin => 1,
            Self::User => 2,
        }
    }

    /// May see the account list.
    pub fn is_admin(self) -> bool {
        matches!(self, Self::SuperAdmin | Self::Admin)
    }

    /// May this role create, change or delete an account that has (or would get) `target`?
    pub fn may_manage(self, target: Role) -> bool {
        match self {
            Self::SuperAdmin => true,
            Self::Admin => target == Self::User,
            Self::User => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Role::*;

    #[test]
    fn super_admins_manage_everyone_and_admins_only_users() {
        for target in [SuperAdmin, Admin, User] {
            assert!(SuperAdmin.may_manage(target));
            assert!(!User.may_manage(target));
        }
        assert!(Admin.may_manage(User));
        assert!(!Admin.may_manage(Admin));
        assert!(!Admin.may_manage(SuperAdmin));
    }
}
