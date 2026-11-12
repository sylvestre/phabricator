//! The `phabricator_user.user` row and the account checks of
//! `PhabricatorUser`.

use phab_core::{Config, Phid};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, bind_all, placeholders, rows};
use phab_policy::Viewer;

pub const USER_COLUMNS: &[&str] = &[
    "id",
    "phid",
    "userName",
    "realName",
    "profileImagePHID",
    "isSystemAgent",
    "isDisabled",
    "isAdmin",
    "isEmailVerified",
    "isApproved",
    "isMailingList",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub phid: Phid,
    pub user_name: String,
    pub real_name: String,
    pub profile_image_phid: Option<Phid>,
    pub is_system_agent: bool,
    pub is_disabled: bool,
    pub is_admin: bool,
    pub is_email_verified: bool,
    pub is_approved: bool,
    pub is_mailing_list: bool,
}

impl User {
    /// Reads the columns of [`USER_COLUMNS`], optionally prefixed (`u.`).
    pub fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            user_name: rows::text(row, "userName")?,
            real_name: rows::text(row, "realName")?,
            profile_image_phid: rows::opt_text(row, "profileImagePHID")?.map(Phid::new),
            is_system_agent: rows::flag(row, "isSystemAgent")?,
            is_disabled: rows::flag(row, "isDisabled")?,
            is_admin: rows::flag(row, "isAdmin")?,
            is_email_verified: rows::flag(row, "isEmailVerified")?,
            is_approved: rows::flag(row, "isApproved")?,
            is_mailing_list: rows::flag(row, "isMailingList")?,
        })
    }

    pub fn viewer(&self) -> Viewer {
        Viewer::user(self.phid.clone(), self.is_admin)
    }

    /// `PhabricatorUser::isUserActivated()` for a logged-in user.
    pub fn is_activated(&self, config: &Config) -> bool {
        if self.is_disabled || !self.is_approved {
            return false;
        }
        let verification_required =
            config.truthy("auth.require-email-verification") || config.truthy("auth.email-domains");
        !verification_required || self.is_email_verified
    }

    /// `PhabricatorUser::canEstablishAPISessions()`, outside of cluster
    /// requests.
    pub fn can_establish_api_sessions(&self, config: &Config) -> bool {
        !self.is_disabled && self.is_activated(config) && !self.is_mailing_list
    }

    /// `PhabricatorUser::canEstablishWebSessions()`.
    pub fn can_establish_web_sessions(&self) -> bool {
        !self.is_mailing_list && !self.is_system_agent
    }
}

pub fn select_columns(alias: &str) -> String {
    USER_COLUMNS
        .iter()
        .map(|column| format!("{alias}.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub async fn load_users_by_phid(database: &Database, phids: &[Phid]) -> Result<Vec<User>, DbError> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {} FROM {} u WHERE u.phid IN ({})",
        select_columns("u"),
        database.table("user", "user"),
        placeholders(phids.len()),
    );
    let rows = bind_all(phab_db::sqlx::query(&sql), phids.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?;
    rows.iter().map(User::from_row).collect()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn user() -> User {
        User {
            id: 1,
            phid: Phid::new("PHID-USER-alice"),
            user_name: "alice".to_owned(),
            real_name: "Alice".to_owned(),
            profile_image_phid: None,
            is_system_agent: false,
            is_disabled: false,
            is_admin: false,
            is_email_verified: true,
            is_approved: true,
            is_mailing_list: false,
        }
    }

    #[rstest]
    #[case(|_: &mut User| {}, true)]
    #[case(|user: &mut User| user.is_disabled = true, false)]
    #[case(|user: &mut User| user.is_approved = false, false)]
    #[case(|user: &mut User| user.is_mailing_list = true, false)]
    #[case(|user: &mut User| user.is_email_verified = false, true)]
    #[case(|user: &mut User| user.is_system_agent = true, true)]
    fn api_sessions(#[case] change: fn(&mut User), #[case] expected: bool) {
        let mut account = user();
        change(&mut account);
        assert_eq!(
            account.can_establish_api_sessions(&Config::default()),
            expected
        );
    }

    #[test]
    fn unverified_email_blocks_when_verification_is_required() {
        let mut config = Config::default();
        config.push_layer(
            "test",
            serde_json::Map::from_iter([("auth.email-domains".to_owned(), json!(["example.com"]))]),
        );
        let mut account = user();
        account.is_email_verified = false;
        assert!(!account.is_activated(&config));
    }

    #[test]
    fn bots_and_lists_cannot_use_the_web() {
        let mut account = user();
        assert!(account.can_establish_web_sessions());
        account.is_system_agent = true;
        assert!(!account.can_establish_web_sessions());
    }
}
