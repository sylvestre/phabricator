//! The Conduit API tokens of the settings panel, as
//! `PhabricatorConduitTokenEditController` and
//! `PhabricatorConduitTokenTerminateController` write them: a new
//! `conduit_token` row (`initializeNewToken()` and `save()`), and tokens
//! expired a minute ago. PHP writes no log or transaction for either.

use std::fmt;

use phab_core::{Phid, random};
use phab_db::{Database, rows};

use crate::AuthError;

/// `PhabricatorConduitToken::TYPE_STANDARD`.
pub const TYPE_STANDARD: &str = "api";
/// `PhabricatorConduitToken::TYPE_CLUSTER`.
pub const TYPE_CLUSTER: &str = "clr";
/// The length of the `token` column (`text32`).
const TOKEN_LENGTH: usize = 32;
/// How far back terminated tokens expire (`PhabricatorTime::getNow() - 60`).
const TERMINATED_AGO: i64 = 60;

/// A token's secret, which only the token's own dialog shows. Never
/// printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The secret itself, for the dialog which shows it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// A `conduit_token` row which has not expired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveToken {
    /// `id`.
    pub id: i64,
    /// The user the token authenticates.
    pub object_phid: Phid,
    /// `tokenType`.
    pub token_type: String,
    /// `token`.
    pub token: Secret,
}

fn live_token(row: &phab_db::sqlx::mysql::MySqlRow) -> Result<LiveToken, AuthError> {
    Ok(LiveToken {
        id: rows::int(row, "id")?,
        object_phid: Phid::new(rows::text(row, "objectPHID")?),
        token_type: rows::text(row, "tokenType")?,
        token: Secret(rows::text(row, "token")?),
    })
}

/// `PhabricatorConduitTokenQuery` with `withIDs(array($id))` and
/// `withExpired(false)`.
pub async fn load_live(
    database: &Database,
    id: i64,
    now: i64,
) -> Result<Option<LiveToken>, AuthError> {
    let sql = format!(
        "SELECT id, objectPHID, tokenType, token FROM {} \
         WHERE id = ? AND (expires IS NULL OR expires > ?)",
        database.table("conduit", "conduit_token")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(id)
        .bind(now)
        .fetch_optional(database.pool())
        .await?;
    found.as_ref().map(live_token).transpose()
}

/// `PhabricatorConduitTokenQuery` with `withObjectPHIDs()` and
/// `withExpired(false)`, newest first.
pub async fn load_live_for_object(
    database: &Database,
    object: &Phid,
    now: i64,
) -> Result<Vec<LiveToken>, AuthError> {
    let sql = format!(
        "SELECT id, objectPHID, tokenType, token FROM {} \
         WHERE objectPHID = ? AND (expires IS NULL OR expires > ?) ORDER BY id DESC",
        database.table("conduit", "conduit_token")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(object.as_str())
        .bind(now)
        .fetch_all(database.pool())
        .await?;
    found.iter().map(live_token).collect()
}

/// `initializeNewToken($object_phid, TYPE_STANDARD)` and `save()`: a
/// token which never expires. Returns its ID.
pub async fn create_standard(
    database: &Database,
    object: &Phid,
    now: i64,
) -> Result<i64, AuthError> {
    // `substr($token_type.'-'.Filesystem::readRandomCharacters(32), 0, 32)`.
    let mut secret = format!("{TYPE_STANDARD}-{}", random::random_characters(32));
    secret.truncate(TOKEN_LENGTH);
    let sql = format!(
        "INSERT INTO {} (objectPHID, tokenType, token, expires, dateCreated, dateModified) \
         VALUES (?, ?, ?, NULL, ?, ?)",
        database.table("conduit", "conduit_token")
    );
    let id = phab_db::sqlx::query(&sql)
        .bind(object.as_str())
        .bind(TYPE_STANDARD)
        .bind(&secret)
        .bind(now)
        .bind(now)
        .execute(database.pool())
        .await?
        .last_insert_id();
    Ok(i64::try_from(id).unwrap_or(i64::MAX))
}

/// `save()` of a loaded token, which changes nothing but `dateModified`.
pub async fn touch(database: &Database, token: &LiveToken, now: i64) -> Result<(), AuthError> {
    let sql = format!(
        "UPDATE {} SET dateModified = ? WHERE id = ?",
        database.table("conduit", "conduit_token")
    );
    phab_db::sqlx::query(&sql)
        .bind(now)
        .bind(token.id)
        .execute(database.pool())
        .await?;
    Ok(())
}

/// `setExpires(PhabricatorTime::getNow() - 60)->save()` of each token.
pub async fn terminate(
    database: &Database,
    tokens: &[LiveToken],
    now: i64,
) -> Result<(), AuthError> {
    let sql = format!(
        "UPDATE {} SET expires = ?, dateModified = ? WHERE id = ?",
        database.table("conduit", "conduit_token")
    );
    for token in tokens {
        phab_db::sqlx::query(&sql)
            .bind(now - TERMINATED_AGO)
            .bind(now)
            .bind(token.id)
            .execute(database.pool())
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_never_printed() {
        let secret = Secret("api-secret".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(secret.expose(), "api-secret");
    }
}
