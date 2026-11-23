//! External accounts (`PhabricatorExternalAccount`) as the login workflow
//! loads, creates and saves them: `newExternalAccountForIdentifiers()`,
//! `newExternalAccountForUser()` and `didUpdateAccount()` of
//! `PhabricatorAuthProvider`.

use std::fmt;

use phab_core::{Phid, PhidType, hash, php, random};
use phab_db::sqlx::MySqlConnection;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, rows};
use serde_json::{Map, Value};

use crate::provider::ProviderConfig;
use crate::{AuthError, Outcome};

/// `PhabricatorPeopleExternalPHIDType`.
const ACCOUNT_PHID_TYPE: PhidType = PhidType::new(*b"XUSR");
/// `PhabricatorPeopleExternalIdentifierPHIDType`.
const IDENTIFIER_PHID_TYPE: PhidType = PhidType::new(*b"XIDT");
/// Length of `accountSecret`, which registration links carry.
const ACCOUNT_SECRET_LENGTH: usize = 32;

/// A `user_externalaccountidentifier` row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identifier {
    /// `None` until saved.
    id: Option<i64>,
    raw: String,
}

/// A `user_externalaccount` row and its identifiers.
#[derive(Clone, PartialEq)]
pub struct ExternalAccount {
    id: Option<i64>,
    /// The account PHID, once saved.
    pub phid: Option<Phid>,
    /// The Phabricator user the account is linked to, if any.
    pub user_phid: Option<Phid>,
    account_secret: Option<String>,
    account_type: String,
    account_domain: String,
    provider_config_phid: Phid,
    /// What the provider says about the account (`didUpdateAccount()`).
    pub profile: AccountProfile,
    /// `emailVerified`, which neither installed provider ever sets.
    pub email_verified: bool,
    properties: Map<String, Value>,
    identifiers: Vec<Identifier>,
}

impl fmt::Debug for ExternalAccount {
    // The account secret is a registration capability and the properties
    // hold OAuth tokens: neither is printed.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalAccount")
            .field("id", &self.id)
            .field("phid", &self.phid)
            .field("user_phid", &self.user_phid)
            .field("provider_config_phid", &self.provider_config_phid)
            .finish_non_exhaustive()
    }
}

/// The account fields `didUpdateAccount()` copies from the adapter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountProfile {
    /// `getAccountName()`.
    pub username: Option<String>,
    /// `getAccountRealName()`.
    pub real_name: Option<String>,
    /// `getAccountEmail()`.
    pub email: Option<String>,
}

/// Why no single account could be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    /// The identifiers map to more than one account; PHP throws.
    Ambiguous,
}

impl ExternalAccount {
    /// `PhabricatorAuthProvider::newExternalAccount()`.
    fn new(provider: &ProviderConfig) -> Self {
        Self {
            id: None,
            phid: None,
            user_phid: None,
            account_secret: None,
            account_type: provider.kind.adapter_type().to_owned(),
            account_domain: provider.kind.adapter_domain().to_owned(),
            provider_config_phid: provider.phid.clone(),
            profile: AccountProfile::default(),
            email_verified: false,
            properties: Map::new(),
            identifiers: Vec::new(),
        }
    }

    fn from_row(row: &MySqlRow) -> Result<Self, AuthError> {
        let properties = match rows::json(row, "properties")? {
            Value::Object(properties) => properties,
            _ => Map::new(),
        };
        Ok(Self {
            id: Some(rows::int(row, "id")?),
            phid: Some(Phid::new(rows::text(row, "phid")?)),
            user_phid: rows::opt_text(row, "userPHID")?.map(Phid::new),
            account_secret: rows::opt_text(row, "accountSecret")?,
            account_type: rows::text(row, "accountType")?,
            account_domain: rows::text(row, "accountDomain")?,
            provider_config_phid: Phid::new(rows::text(row, "providerConfigPHID")?),
            profile: AccountProfile {
                username: rows::opt_text(row, "username")?,
                real_name: rows::opt_text(row, "realName")?,
                email: rows::opt_text(row, "email")?,
            },
            email_verified: rows::flag(row, "emailVerified")?,
            properties,
            identifiers: Vec::new(),
        })
    }

    /// The account secret, which registration links carry. Set once
    /// saved.
    pub fn account_secret(&self) -> Option<&str> {
        self.account_secret.as_deref()
    }

    /// The provider configuration the account belongs to.
    pub fn provider_config_phid(&self) -> &Phid {
        &self.provider_config_phid
    }

    /// `setProperty()`.
    pub fn set_property(&mut self, key: &str, value: Value) {
        self.properties.insert(key.to_owned(), value);
    }

    /// `getProperty()`.
    pub fn property(&self, key: &str) -> Option<&Value> {
        self.properties.get(key)
    }

    /// `appendIdentifier()`: an identifier the account already has is kept.
    fn append_identifier(&mut self, raw: &str) {
        if !self.identifiers.iter().any(|known| known.raw == raw) {
            self.identifiers.push(Identifier {
                id: None,
                raw: raw.to_owned(),
            });
        }
    }
}

const ACCOUNT_COLUMNS: &str = "account.id, account.phid, account.userPHID, \
    account.accountSecret, account.accountType, account.accountDomain, \
    account.providerConfigPHID, account.username, account.realName, account.email, \
    account.emailVerified, account.properties, account.accountURI, account.profileImagePHID";

/// The identifiers of `account`, newest first like
/// `PhabricatorExternalAccountIdentifierQuery`.
async fn load_identifiers(
    database: &Database,
    account: &mut ExternalAccount,
) -> Result<(), AuthError> {
    let Some(phid) = &account.phid else {
        return Ok(());
    };
    let sql = format!(
        "SELECT id, identifierRaw FROM {} WHERE externalAccountPHID = ? ORDER BY id DESC",
        database.table("user", "user_externalaccountidentifier")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(phid.as_str())
        .fetch_all(database.pool())
        .await?;
    for row in &found {
        let raw = rows::text(row, "identifierRaw")?;
        // `attachAccountIdentifiers()` keys them by raw value.
        account.identifiers.retain(|known| known.raw != raw);
        account.identifiers.push(Identifier {
            id: Some(rows::int(row, "id")?),
            raw,
        });
    }
    Ok(())
}

/// `newExternalAccountForIdentifiers()` up to `didUpdateAccount()`: the
/// account of `provider` with one of `identifiers`, or a new one, with
/// every identifier attached. Not saved yet.
pub async fn for_identifiers(
    database: &Database,
    provider: &ProviderConfig,
    identifiers: &[String],
) -> Result<Result<ExternalAccount, AccountError>, AuthError> {
    let sql = format!(
        "SELECT {ACCOUNT_COLUMNS} FROM {} account \
         JOIN {} identifier ON account.phid = identifier.externalAccountPHID \
         WHERE account.providerConfigPHID IN (?) AND identifier.providerConfigPHID IN (?) \
         AND identifier.identifierHash IN ({}) GROUP BY account.id ORDER BY account.id DESC",
        database.table("user", "user_externalaccount"),
        database.table("user", "user_externalaccountidentifier"),
        phab_db::placeholders(identifiers.len()),
    );
    let mut query = phab_db::sqlx::query(&sql)
        .bind(provider.phid.as_str())
        .bind(provider.phid.as_str());
    for identifier in identifiers {
        query = query.bind(hash::digest_for_index(identifier.as_bytes()));
    }
    let found = query.fetch_all(database.pool()).await?;
    let mut account = match found.as_slice() {
        [] => ExternalAccount::new(provider),
        [row] => {
            let mut account = ExternalAccount::from_row(row)?;
            load_identifiers(database, &mut account).await?;
            account
        }
        _ => return Ok(Err(AccountError::Ambiguous)),
    };
    for identifier in identifiers {
        account.append_identifier(identifier);
    }
    Ok(Ok(account))
}

/// `PhabricatorExternalAccountQuery::withAccountSecrets()`: the account a
/// registration or link URI names. `Unsupported` for accounts with a
/// profile image or an account URI, which [`save()`] would clear where
/// PHP writes them back, and where PHP's `executeOne()` throws.
pub async fn for_secret(
    database: &Database,
    secret: &str,
) -> Result<Outcome<Option<ExternalAccount>>, AuthError> {
    let sql = format!(
        "SELECT {ACCOUNT_COLUMNS} FROM {} account WHERE account.accountSecret IN (?) \
         ORDER BY account.id DESC",
        database.table("user", "user_externalaccount"),
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(secret)
        .fetch_all(database.pool())
        .await?;
    let row = match found.as_slice() {
        [] => return Ok(Outcome::Done(None)),
        [row] => row,
        _ => return Ok(Outcome::Unsupported("two external accounts share a secret")),
    };
    if rows::opt_text(row, "accountURI")?.is_some()
        || rows::opt_text(row, "profileImagePHID")?.is_some()
    {
        return Ok(Outcome::Unsupported(
            "an external account with a profile image or URI",
        ));
    }
    Ok(Outcome::Done(Some(ExternalAccount::from_row(row)?)))
}

/// Whether `user` has an account on `provider`
/// (`withUserPHIDs()->withProviderConfigPHIDs()`).
pub async fn user_has_account(
    database: &Database,
    provider: &ProviderConfig,
    user: &Phid,
) -> Result<bool, AuthError> {
    let sql = format!(
        "SELECT 1 FROM {} WHERE userPHID IN (?) AND providerConfigPHID IN (?) LIMIT 1",
        database.table("user", "user_externalaccount"),
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .bind(provider.phid.as_str())
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// `newExternalAccountForUser()` up to `didUpdateAccount()`: the account
/// of `provider` linked to `user`, or a new one. Not saved yet.
pub async fn for_user(
    database: &Database,
    provider: &ProviderConfig,
    user: &Phid,
) -> Result<Result<ExternalAccount, AccountError>, AuthError> {
    let sql = format!(
        "SELECT {ACCOUNT_COLUMNS} FROM {} account \
         WHERE account.providerConfigPHID IN (?) AND account.userPHID IN (?) \
         ORDER BY account.id DESC",
        database.table("user", "user_externalaccount"),
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(provider.phid.as_str())
        .bind(user.as_str())
        .fetch_all(database.pool())
        .await?;
    match found.as_slice() {
        [] => {
            let mut account = ExternalAccount::new(provider);
            account.user_phid = Some(user.clone());
            Ok(Ok(account))
        }
        // `executeOne()` throws on more than one result.
        [row] => Ok(Ok(ExternalAccount::from_row(row)?)),
        _ => Ok(Err(AccountError::Ambiguous)),
    }
}

fn properties_json(properties: &Map<String, Value>) -> String {
    // PHP encodes an empty array as a list.
    if properties.is_empty() {
        return "[]".to_owned();
    }
    php::json_encode(&Value::Object(properties.clone()))
}

/// `PhabricatorExternalAccount::save()`: inserts or updates the account,
/// then saves each identifier, in one transaction. `didUpdateAccount()`
/// clears the profile image and the account URI, which BMO and passwords
/// never provide.
pub async fn save(
    database: &Database,
    account: &mut ExternalAccount,
    now: i64,
) -> Result<(), AuthError> {
    let mut transaction = database.pool().begin().await?;
    save_on(&mut transaction, database, account, now).await?;
    transaction.commit().await?;
    Ok(())
}

/// [`save()`] on a connection whose transaction the caller holds.
pub async fn save_on(
    transaction: &mut MySqlConnection,
    database: &Database,
    account: &mut ExternalAccount,
    now: i64,
) -> Result<(), AuthError> {
    let accounts = database.table("user", "user_externalaccount");
    let identifiers = database.table("user", "user_externalaccountidentifier");
    if account.account_secret.as_deref().is_none_or(str::is_empty) {
        account.account_secret = Some(random::random_characters(ACCOUNT_SECRET_LENGTH));
    }
    let properties = properties_json(&account.properties);

    if let Some(id) = account.id {
        let sql = format!(
            "UPDATE {accounts} SET userPHID = ?, accountSecret = ?, username = ?, \
               realName = ?, email = ?, accountURI = NULL, profileImagePHID = NULL, \
               properties = ?, dateModified = ? WHERE id = ?"
        );
        phab_db::sqlx::query(&sql)
            .bind(account.user_phid.as_ref().map(Phid::as_str))
            .bind(&account.account_secret)
            .bind(&account.profile.username)
            .bind(&account.profile.real_name)
            .bind(&account.profile.email)
            .bind(&properties)
            .bind(now)
            .bind(id)
            .execute(&mut *transaction)
            .await?;
    } else {
        let phid = Phid::generate(ACCOUNT_PHID_TYPE, None);
        let sql = format!(
            "INSERT INTO {accounts} (phid, userPHID, accountType, accountDomain, \
               accountSecret, accountID, displayName, dateCreated, dateModified, username, \
               realName, email, emailVerified, accountURI, profileImagePHID, properties, \
               providerConfigPHID) \
             VALUES (?, ?, ?, ?, ?, '', NULL, ?, ?, ?, ?, ?, 0, NULL, NULL, ?, ?)"
        );
        let result = phab_db::sqlx::query(&sql)
            .bind(phid.as_str())
            .bind(account.user_phid.as_ref().map(Phid::as_str))
            .bind(&account.account_type)
            .bind(&account.account_domain)
            .bind(&account.account_secret)
            .bind(now)
            .bind(now)
            .bind(&account.profile.username)
            .bind(&account.profile.real_name)
            .bind(&account.profile.email)
            .bind(&properties)
            .bind(account.provider_config_phid.as_str())
            .execute(&mut *transaction)
            .await?;
        account.id = Some(i64::try_from(result.last_insert_id()).unwrap_or(i64::MAX));
        account.phid = Some(phid);
    }

    let account_phid = account.phid.clone().unwrap_or_else(|| Phid::new(""));
    for identifier in &mut account.identifiers {
        let digest = hash::digest_for_index(identifier.raw.as_bytes());
        if let Some(id) = identifier.id {
            let sql = format!(
                "UPDATE {identifiers} SET externalAccountPHID = ?, providerConfigPHID = ?, \
                   identifierHash = ?, identifierRaw = ?, dateModified = ? WHERE id = ?"
            );
            phab_db::sqlx::query(&sql)
                .bind(account_phid.as_str())
                .bind(account.provider_config_phid.as_str())
                .bind(&digest)
                .bind(&identifier.raw)
                .bind(now)
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        } else {
            let sql = format!(
                "INSERT INTO {identifiers} (phid, externalAccountPHID, providerConfigPHID, \
                   identifierHash, identifierRaw, dateCreated, dateModified) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)"
            );
            let result = phab_db::sqlx::query(&sql)
                .bind(Phid::generate(IDENTIFIER_PHID_TYPE, None).as_str())
                .bind(account_phid.as_str())
                .bind(account.provider_config_phid.as_str())
                .bind(&digest)
                .bind(&identifier.raw)
                .bind(now)
                .bind(now)
                .execute(&mut *transaction)
                .await?;
            identifier.id = Some(i64::try_from(result.last_insert_id()).unwrap_or(i64::MAX));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_properties_encode_like_php() {
        assert_eq!(properties_json(&Map::new()), "[]");
        let properties = Map::from_iter([(
            "oauth.token.access".to_owned(),
            Value::String("a/b".to_owned()),
        )]);
        assert_eq!(
            properties_json(&properties),
            r#"{"oauth.token.access":"a\/b"}"#
        );
    }
}
