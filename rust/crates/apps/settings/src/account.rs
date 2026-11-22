//! What the list panels of a user's settings show: email addresses,
//! sessions, Conduit API tokens and authentication factors.

use phab_core::Phid;
use phab_db::{Database, DbError, rows};

use crate::Error;

/// A row of `user_email`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAddress {
    /// `id`, which the panel's actions name.
    pub id: i64,
    /// The address.
    pub address: String,
    /// `isVerified`.
    pub is_verified: bool,
    /// `isPrimary`.
    pub is_primary: bool,
}

/// `PhabricatorEmailAddressesSettingsPanel`: the addresses of `user`,
/// sorted by address.
pub async fn email_addresses(database: &Database, user: &Phid) -> Result<Vec<EmailAddress>, Error> {
    let sql = format!(
        "SELECT id, address, isVerified, isPrimary FROM {} WHERE userPHID = ? ORDER BY address",
        database.table("user", "user_email")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    found
        .iter()
        .map(|row| {
            Ok(EmailAddress {
                id: rows::int(row, "id")?,
                address: rows::text(row, "address")?,
                is_verified: rows::flag(row, "isVerified")?,
                is_primary: rows::flag(row, "isPrimary")?,
            })
        })
        .collect()
}

/// A row of `phabricator_session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// `id`.
    pub id: i64,
    /// The first characters of the key digest, which the panel shows.
    pub key_prefix: String,
    /// `type`.
    pub session_type: String,
    /// `sessionStart`.
    pub start: i64,
    /// `sessionExpires`.
    pub expires: i64,
    /// `highSecurityUntil`.
    pub high_security_until: Option<i64>,
}

/// `PhabricatorSessionsSettingsPanel`: the sessions of `user` and of
/// their external accounts (`withIdentityPHIDs()`), newest first
/// (`PhabricatorAuthSessionQuery`). Sessions of external accounts, which
/// PHP doesn't open, aren't ported.
pub async fn sessions(database: &Database, user: &Phid) -> Result<Vec<SessionRow>, Error> {
    let sql = format!(
        "SELECT id, userPHID, sessionKey, type, sessionStart, sessionExpires, highSecurityUntil \
         FROM {} WHERE userPHID = ? OR userPHID IN (SELECT phid FROM {} WHERE userPHID = ?) \
         ORDER BY id DESC",
        database.table("user", "phabricator_session"),
        database.table("user", "user_externalaccount"),
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .bind(user.as_str())
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    found
        .iter()
        .map(|row| {
            if rows::text(row, "userPHID")? != user.as_str() {
                return Err(Error::Unsupported("the sessions of external accounts"));
            }
            let key = rows::text(row, "sessionKey")?;
            Ok(SessionRow {
                id: rows::int(row, "id")?,
                key_prefix: key.chars().take(6).collect(),
                session_type: rows::text(row, "type")?,
                start: rows::int(row, "sessionStart")?,
                expires: rows::int(row, "sessionExpires")?,
                high_security_until: rows::opt_int(row, "highSecurityUntil")?,
            })
        })
        .collect()
}

/// A row of `conduit_token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConduitToken {
    /// `id`.
    pub id: i64,
    /// `getPublicTokenName()`.
    pub name: String,
    /// `getTokenTypeName()`.
    pub type_name: String,
    /// `dateCreated`.
    pub created: i64,
    /// `expires`.
    pub expires: Option<i64>,
}

/// `PhabricatorConduitTokensSettingsPanel`: the tokens of `user` which
/// have not expired, newest first.
pub async fn conduit_tokens(
    database: &Database,
    user: &Phid,
    now: i64,
) -> Result<Vec<ConduitToken>, Error> {
    let sql = format!(
        "SELECT id, tokenType, token, dateCreated, expires FROM {} \
         WHERE objectPHID = ? AND (expires IS NULL OR expires > ?) ORDER BY id DESC",
        database.table("conduit", "conduit_token")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .bind(now)
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    found
        .iter()
        .map(|row| {
            let kind = rows::text(row, "tokenType")?;
            let token = rows::text(row, "token")?;
            let (name, type_name) = match kind.as_str() {
                "clr" => (
                    "Cluster API Token".to_owned(),
                    "Cluster API Token".to_owned(),
                ),
                "api" => (
                    format!("{}...", token.chars().take(8).collect::<String>()),
                    "Standard API Token".to_owned(),
                ),
                "cli" => (
                    format!("{}...", token.chars().take(8).collect::<String>()),
                    "Command Line API Token".to_owned(),
                ),
                _ => (
                    format!("{}...", token.chars().take(8).collect::<String>()),
                    kind.clone(),
                ),
            };
            Ok(ConduitToken {
                id: rows::int(row, "id")?,
                name,
                type_name,
                created: rows::int(row, "dateCreated")?,
                expires: rows::opt_int(row, "expires")?,
            })
        })
        .collect()
}

/// The status of a factor provider (`PhabricatorAuthFactorProviderStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProviderStatus {
    /// `active`.
    Active,
    /// `deprecated`.
    Deprecated,
    /// `disabled`.
    Disabled,
    /// Anything else, which PHP sorts first.
    Unknown,
}

impl ProviderStatus {
    fn from_key(key: &str) -> Self {
        match key {
            "active" => Self::Active,
            "deprecated" => Self::Deprecated,
            "disabled" => Self::Disabled,
            _ => Self::Unknown,
        }
    }

    /// `getOrder()`.
    fn order(self) -> i64 {
        match self {
            Self::Unknown => 0,
            Self::Active => 1,
            Self::Deprecated => 2,
            Self::Disabled => 3,
        }
    }

    /// `getName()`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Deprecated => "Deprecated",
            Self::Disabled => "Disabled",
            Self::Unknown => "Unknown",
        }
    }
}

/// An authentication factor of the user, with its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Factor {
    /// `id`.
    pub id: i64,
    /// `factorName`.
    pub name: String,
    /// The provider's status.
    pub status: ProviderStatus,
    /// `getDisplayName()` of the provider.
    pub provider_name: String,
    /// `getConfigurationListDetails()`.
    pub details: String,
    /// `dateCreated`.
    pub created: i64,
}

/// `PhabricatorMultiFactorSettingsPanel`: the factors of `user` in
/// `newSortVector()` order, and whether some provider is active (the "Add
/// Auth Factor" button). Only TOTP factors are ported.
pub async fn factors(database: &Database, user: &Phid) -> Result<(Vec<Factor>, bool), Error> {
    let providers = database.table("auth", "auth_factorprovider");
    let sql = format!(
        "SELECT config.id, config.factorName, config.factorSecret, config.dateCreated, \
           provider.providerFactorKey, provider.status, provider.name \
         FROM {} config JOIN {providers} provider ON config.factorProviderPHID = provider.phid \
         WHERE config.userPHID = ?",
        database.table("auth", "auth_factorconfig")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    let mut factors = Vec::with_capacity(found.len());
    for row in &found {
        if rows::text(row, "providerFactorKey")? != "totp" {
            return Err(Error::Unsupported("factors other than TOTP"));
        }
        let name = rows::text(row, "name")?;
        factors.push(Factor {
            id: rows::int(row, "id")?,
            name: rows::text(row, "factorName")?,
            status: ProviderStatus::from_key(&rows::text(row, "status")?),
            provider_name: if name.is_empty() {
                "Mobile Phone App (TOTP)".to_owned()
            } else {
                name
            },
            details: format!("{}-Bit Secret", rows::text(row, "factorSecret")?.len() * 8),
            created: rows::int(row, "dateCreated")?,
        });
    }
    factors.sort_by_key(|factor| (factor.status.order(), factor.id));
    let sql = format!("SELECT id FROM {providers} WHERE status = 'active' LIMIT 1");
    let can_add = phab_db::sqlx::query(&sql)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?
        .is_some();
    Ok((factors, can_add))
}

/// `PhabricatorPasswordAuthProvider::getPasswordProvider()`: whether
/// password logins are enabled, which shows the Password panel.
pub async fn has_password_provider(database: &Database) -> Result<bool, Error> {
    let sql = format!(
        "SELECT id FROM {} WHERE providerClass = 'PhabricatorPasswordAuthProvider' \
         AND isEnabled = 1 LIMIT 1",
        database.table("auth", "auth_providerconfig")
    );
    Ok(phab_db::sqlx::query(&sql)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?
        .is_some())
}
