//! BMO external accounts, as `PhabricatorExternalAccountQuery` loads them.
//!
//! External accounts and their identifiers are visible to every logged-in
//! user (their view policy is the most open one), so loading them for a
//! Conduit caller needs no policy filtering.

use std::collections::HashMap;

use phab_core::{Phid, hash};
use phab_db::{Database, DbError, rows};

/// The provider class of BMO logins.
const BMO_PROVIDER: &str = "PhabricatorBMOAuthProvider";

/// A `user_externalaccount` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAccount {
    /// The account's PHID.
    pub phid: Phid,
    /// The Phabricator user the account is linked to, if any.
    pub user_phid: Option<String>,
}

/// How the accounts are selected.
#[derive(Debug, Clone, Copy)]
pub enum AccountFilter<'a> {
    /// `withRawAccountIdentifiers()`: accounts with one of these
    /// identifiers, such as BMO user ids.
    RawIdentifiers(&'a [String]),
    /// `withUserPHIDs()`: accounts linked to one of these users.
    UserPhids(&'a [String]),
}

/// The PHID of the BMO provider configuration, or `None` unless there is
/// exactly one, where PHP's `executeOne()` fails.
pub async fn bmo_provider_config(database: &Database) -> Result<Option<Phid>, DbError> {
    let sql = format!(
        "SELECT phid FROM {} WHERE providerClass IN (?)",
        database.table("auth", "auth_providerconfig")
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(BMO_PROVIDER)
        .fetch_all(database.pool())
        .await?;
    match rows.as_slice() {
        [row] => Ok(Some(Phid::new(rows::text(row, "phid")?))),
        _ => Ok(None),
    }
}

/// The accounts of `provider` matching `filter`, newest first, like the
/// query's default order.
pub async fn load_accounts(
    database: &Database,
    provider: &Phid,
    filter: AccountFilter<'_>,
) -> Result<Vec<ExternalAccount>, DbError> {
    let accounts = database.table("user", "user_externalaccount");
    let (sql, values): (String, Vec<String>) = match filter {
        AccountFilter::RawIdentifiers(identifiers) => (
            format!(
                "SELECT account.phid, account.userPHID FROM {accounts} account \
                 JOIN {} identifier ON account.phid = identifier.externalAccountPHID \
                 WHERE account.providerConfigPHID IN (?) \
                 AND identifier.providerConfigPHID IN (?) \
                 AND identifier.identifierHash IN ({}) \
                 GROUP BY account.id ORDER BY account.id DESC",
                database.table("user", "user_externalaccountidentifier"),
                phab_db::placeholders(identifiers.len()),
            ),
            std::iter::repeat_n(provider.as_str().to_owned(), 2)
                .chain(
                    identifiers
                        .iter()
                        .map(|identifier| hash::digest_for_index(identifier.as_bytes())),
                )
                .collect(),
        ),
        AccountFilter::UserPhids(users) => (
            format!(
                "SELECT account.phid, account.userPHID FROM {accounts} account \
                 WHERE account.providerConfigPHID IN (?) AND account.userPHID IN ({}) \
                 ORDER BY account.id DESC",
                phab_db::placeholders(users.len()),
            ),
            std::iter::once(provider.as_str().to_owned())
                .chain(users.iter().cloned())
                .collect(),
        ),
    };
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), values)
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| {
            Ok(ExternalAccount {
                phid: Phid::new(rows::text(row, "phid")?),
                user_phid: rows::opt_text(row, "userPHID")?,
            })
        })
        .collect()
}

/// The raw identifier the method reports for each account: the first one
/// `PhabricatorExternalAccountIdentifierQuery` returns, which is the newest.
pub async fn first_identifiers(
    database: &Database,
    accounts: &[ExternalAccount],
) -> Result<HashMap<Phid, String>, DbError> {
    if accounts.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT externalAccountPHID, identifierRaw FROM {} \
         WHERE externalAccountPHID IN ({}) ORDER BY id DESC",
        database.table("user", "user_externalaccountidentifier"),
        phab_db::placeholders(accounts.len()),
    );
    let rows = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        accounts.iter().map(|account| account.phid.as_str()),
    )
    .fetch_all(database.pool())
    .await?;
    let mut identifiers = HashMap::new();
    for row in rows {
        let account = Phid::new(rows::text(&row, "externalAccountPHID")?);
        let raw = rows::text(&row, "identifierRaw")?;
        identifiers.entry(account).or_insert(raw);
    }
    Ok(identifiers)
}
