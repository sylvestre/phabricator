//! The `phabricator_user.user` row and the account checks of
//! `PhabricatorUser`.

use std::fmt::Write as _;

use phab_core::{Config, Phid};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::Viewer;

/// The `user` columns [`User::from_row()`] reads.
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

/// A user account, as much of `PhabricatorUser` as authentication needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// The row ID.
    pub id: i64,
    /// The user PHID.
    pub phid: Phid,
    /// The username.
    pub user_name: String,
    /// The real name, possibly empty.
    pub real_name: String,
    /// The profile picture file, if one was set.
    pub profile_image_phid: Option<Phid>,
    /// Whether the account is a bot.
    pub is_system_agent: bool,
    /// Whether the account is disabled.
    pub is_disabled: bool,
    /// Whether the account is an administrator.
    pub is_admin: bool,
    /// Whether the primary email address is verified.
    pub is_email_verified: bool,
    /// Whether an administrator approved the account.
    pub is_approved: bool,
    /// Whether the account is a mailing list.
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

    /// The policy viewer acting as this user.
    pub fn viewer(&self) -> Viewer {
        Viewer::user(self.phid.clone(), self.is_admin)
    }

    /// `PhabricatorUser::isUserActivated()` for a logged-in user.
    pub fn is_activated(&self, config: &Config) -> bool {
        !self.is_disabled && self.is_approved && !self.must_verify_email(config)
    }

    /// Whether the user must verify their email address before using
    /// anything (`PhabricatorUserEmail::isEmailVerificationRequired()`).
    pub fn must_verify_email(&self, config: &Config) -> bool {
        let verification_required =
            config.truthy("auth.require-email-verification") || config.truthy("auth.email-domains");
        verification_required && !self.is_email_verified
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

/// Text PHP and Rust lowercase and split alike: printable ASCII.
pub fn is_plain(text: &str) -> bool {
    text.bytes().all(|byte| (b' '..=b'~').contains(&byte))
}

/// `PhabricatorTypeaheadDatasource::tokenizeString()` of plain text (see
/// [`is_plain()`]): the `user_nametoken` rows of a user.
pub fn name_tokens(text: &str) -> Vec<String> {
    let lowered = text.to_ascii_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    for token in lowered
        .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0b'])
        .split(|character: char| character.is_ascii_whitespace() || "[]()-\x0b".contains(character))
    {
        if !token.is_empty() && !tokens.iter().any(|known| known == token) {
            tokens.push(token.to_owned());
        }
    }
    tokens
}

/// [`USER_COLUMNS`] prefixed with `alias.`, for a `SELECT` list.
pub fn select_columns(alias: &str) -> String {
    USER_COLUMNS
        .iter()
        .map(|column| format!("{alias}.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Loads the users with these PHIDs; unknown PHIDs are skipped.
pub async fn load_users_by_phid(database: &Database, phids: &[Phid]) -> Result<Vec<User>, DbError> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {} FROM {} u WHERE u.phid IN ({})",
        select_columns("u"),
        database.table("user", "user"),
        phab_db::placeholders(phids.len()),
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?;
    rows.iter().map(User::from_row).collect()
}

/// Loads the user with this ID, if any.
pub async fn load_user_by_id(database: &Database, id: i64) -> Result<Option<User>, DbError> {
    let sql = format!(
        "SELECT {} FROM {} u WHERE u.id = ?",
        select_columns("u"),
        database.table("user", "user"),
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await?;
    row.as_ref().map(User::from_row).transpose()
}

/// `PhabricatorAuthMessage::loadMessageText()` without the default text:
/// the message an administrator wrote for `key`, if any.
pub async fn load_message_text(database: &Database, key: &str) -> Result<Option<String>, DbError> {
    let sql = format!(
        "SELECT messageText FROM {} WHERE messageKey = ?",
        database.table("auth", "auth_message"),
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(key)
        .fetch_optional(database.pool())
        .await?;
    Ok(row
        .map(|row| rows::text(&row, "messageText"))
        .transpose()?
        .filter(|text| !text.is_empty()))
}

/// `PhabricatorUser::updateMultiFactorEnrollment()`, without the write:
/// whether the user has a factor whose provider is active or deprecated.
/// PHP caches the answer in `isEnrolledInMultiFactor`; this reads the
/// factors, so it is never stale.
pub async fn is_enrolled_in_multi_factor(
    database: &Database,
    user: &Phid,
) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT 1 FROM {} config JOIN {} provider \
         ON provider.phid = config.factorProviderPHID \
         WHERE config.userPHID = ? AND provider.status IN ('active', 'deprecated') LIMIT 1",
        database.table("auth", "auth_factorconfig"),
        database.table("auth", "auth_factorprovider"),
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// Up to `limit` users whose username starts with `prefix` (without case,
/// like MySQL's collation), excluding disabled users and mailing lists, as
/// `PhabricatorPeopleDatasource` offers them; by username.
pub async fn search_by_username_prefix(
    database: &Database,
    prefix: &str,
    limit: u32,
) -> Result<Vec<User>, DbError> {
    let sql = format!(
        "SELECT {} FROM {} u WHERE u.userName LIKE ? AND u.isDisabled = 0 \
         AND u.isMailingList = 0 ORDER BY u.userName LIMIT ?",
        select_columns("u"),
        database.table("user", "user"),
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(format!("{}%", phab_db::like_escape(prefix)))
        .bind(limit)
        .fetch_all(database.pool())
        .await?;
    rows.iter().map(User::from_row).collect()
}

/// `PhabricatorPeopleQuery` as `PhabricatorPeopleDatasource` runs it:
/// users whose username starts with `prefix` (`withNamePrefixes()`), or
/// with a name token starting with each of `tokens` (`withNameTokens()`),
/// by username, at most `limit` of them. Users are visible to everyone.
pub async fn search_for_datasource(
    database: &Database,
    prefix: Option<&str>,
    tokens: &[String],
    limit: u32,
) -> Result<Vec<User>, DbError> {
    let mut joins = String::new();
    let mut binds: Vec<String> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        write!(
            joins,
            " JOIN {table} token_{index} ON token_{index}.userID = u.id \
             AND token_{index}.token LIKE ?",
            table = database.table("user", "user_nametoken"),
        )
        .expect("writing to a String");
        binds.push(format!("{}%", phab_db::like_escape(token)));
    }
    let mut sql = format!(
        "SELECT {} FROM {} u{joins}",
        select_columns("u"),
        database.table("user", "user"),
    );
    if let Some(prefix) = prefix {
        sql.push_str(" WHERE u.userName LIKE ?");
        binds.push(format!("{}%", phab_db::like_escape(prefix)));
    }
    if !tokens.is_empty() {
        sql.push_str(" GROUP BY u.id");
    }
    sql.push_str(" ORDER BY u.userName LIMIT ?");
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), binds)
        .bind(limit)
        .fetch_all(database.pool())
        .await?;
    rows.iter().map(User::from_row).collect()
}

/// Whether any of these users is away (`getAwayUntil()` after
/// `needAvailability()`): `Some(true)` when one has a valid availability
/// cache saying so, `None` when a cache must be rebuilt from calendar
/// events the user is invited to, which only PHP does. Users without a
/// valid cache and without invitations are available, as the rebuilt
/// cache would say.
pub async fn any_away(
    database: &Database,
    users: &[Phid],
    now: i64,
) -> Result<Option<bool>, DbError> {
    if users.is_empty() {
        return Ok(Some(false));
    }
    let sql = format!(
        "SELECT phid, availabilityCache, availabilityCacheTTL FROM {} WHERE phid IN ({})",
        database.table("user", "user"),
        phab_db::placeholders(users.len())
    );
    let mut rebuild = Vec::new();
    let mut away = false;
    for row in phab_db::bind_all(phab_db::sqlx::query(&sql), users.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?
    {
        let ttl = rows::opt_int(&row, "availabilityCacheTTL")?;
        let cache = rows::opt_text(&row, "availabilityCache")?
            .and_then(|cache| serde_json::from_str::<serde_json::Value>(&cache).ok());
        match cache.filter(|_| ttl.is_some_and(|ttl| ttl > now)) {
            Some(cache) => {
                away |= cache.get("until").is_some_and(phab_core::php::is_truthy);
            }
            None => rebuild.push(rows::text(&row, "phid")?),
        }
    }
    if !rebuild.is_empty() {
        let sql = format!(
            "SELECT 1 FROM {} WHERE inviteePHID IN ({}) LIMIT 1",
            database.table("calendar", "calendar_eventinvitee"),
            phab_db::placeholders(rebuild.len())
        );
        if phab_db::bind_all(phab_db::sqlx::query(&sql), rebuild)
            .fetch_optional(database.pool())
            .await?
            .is_some()
        {
            return Ok(None);
        }
    }
    Ok(Some(away))
}

/// Loads the users with these usernames, compared without case like
/// MySQL's collation does; unknown names are skipped.
pub async fn load_users_by_username(
    database: &Database,
    names: &[&str],
) -> Result<Vec<User>, DbError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {} FROM {} u WHERE u.userName IN ({})",
        select_columns("u"),
        database.table("user", "user"),
        phab_db::placeholders(names.len()),
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), names.iter().copied())
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
