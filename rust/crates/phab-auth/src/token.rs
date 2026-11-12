//! Conduit API token authentication, as done by
//! `PhabricatorConduitAPIController::authenticateUser()` for `api.token`.

use phab_core::{Config, Phid, PhidType};

use crate::{AuthError, Outcome, User, Writes, user};
use phab_db::{Database, rows};

pub const TOKEN_TYPES: &[&str] = &["api", "cli", "clr"];

/// An `ERR-INVALID-AUTH` answer, with PHP's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidAuth(pub String);

struct TokenRow {
    id: i64,
    token_type: String,
    object_phid: Phid,
    expires: Option<i64>,
}

/// Format checks done before touching the database.
pub fn check_token_format(token: &str) -> Result<(), InvalidAuth> {
    if token.len() != 32 {
        return Err(InvalidAuth(format!(
            "API token \"{token}\" has the wrong length. API tokens should be 32 characters long."
        )));
    }
    let token_type = token.split('-').next().unwrap_or_default();
    if !TOKEN_TYPES.contains(&token_type) {
        return Err(InvalidAuth(format!(
            "API token \"{token}\" has the wrong format. API tokens should be 32 characters \
             long and begin with one of these prefixes: {}.",
            TOKEN_TYPES.join(", ")
        )));
    }
    Ok(())
}

async fn find_token(
    database: &Database,
    token: &str,
    now: i64,
    expired: bool,
) -> Result<Option<TokenRow>, AuthError> {
    let expiry = if expired {
        "expires <= ?"
    } else {
        "(expires IS NULL OR expires > ?)"
    };
    let sql = format!(
        "SELECT id, tokenType, objectPHID, expires FROM {} WHERE (token IN (?)) AND {expiry} \
         ORDER BY id DESC",
        database.table("conduit", "conduit_token"),
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(token)
        .bind(now)
        .fetch_optional(database.pool())
        .await?;
    row.map(|row| {
        Ok(TokenRow {
            id: rows::int(&row, "id")?,
            token_type: rows::text(&row, "tokenType")?,
            object_phid: Phid::new(rows::text(&row, "objectPHID")?),
            expires: rows::opt_int(&row, "expires")?,
        })
    })
    .transpose()
}

/// The user a token belongs to, as `PhabricatorConduitTokenQuery` loads it.
/// `Unsupported` when the token belongs to something other than a user,
/// which PHP loads through the generic object query.
async fn token_owner(
    database: &Database,
    token: &TokenRow,
) -> Result<Outcome<Option<User>>, AuthError> {
    if token.object_phid.phid_type() != PhidType::USER {
        return Ok(Outcome::Unsupported("API token owned by a non-user object"));
    }
    let users =
        user::load_users_by_phid(database, std::slice::from_ref(&token.object_phid)).await?;
    Ok(Outcome::Done(users.into_iter().next()))
}

/// Authenticates `token`. The inner `Err` is the `ERR-INVALID-AUTH` answer
/// PHP gives.
pub async fn authenticate_token(
    database: &Database,
    config: &Config,
    token: &str,
    now: i64,
    writes: Writes,
) -> Result<Outcome<Result<User, InvalidAuth>>, AuthError> {
    if let Err(invalid) = check_token_format(token) {
        return Ok(Outcome::Done(Err(invalid)));
    }

    let mut found = None;
    if let Some(row) = find_token(database, token, now, false).await? {
        match token_owner(database, &row).await? {
            Outcome::Unsupported(reason) => return Ok(Outcome::Unsupported(reason)),
            Outcome::Done(Some(user)) => found = Some((row, user)),
            Outcome::Done(None) => {}
        }
    }

    let Some((row, user)) = found else {
        let mut previously_valid = false;
        if let Some(row) = find_token(database, token, now, true).await? {
            match token_owner(database, &row).await? {
                Outcome::Unsupported(reason) => return Ok(Outcome::Unsupported(reason)),
                Outcome::Done(owner) => previously_valid = owner.is_some(),
            }
        }
        let message = if previously_valid {
            format!("API token \"{token}\" was previously valid, but has expired.")
        } else {
            format!("API token \"{token}\" is not valid.")
        };
        return Ok(Outcome::Done(Err(InvalidAuth(message))));
    };

    // A "cli-" token becomes permanent the first time it is used.
    if row.token_type == "cli" && row.expires.is_some() && writes == Writes::Allowed {
        let sql = format!(
            "UPDATE {} SET expires = NULL, dateModified = ? WHERE id = ?",
            database.table("conduit", "conduit_token")
        );
        phab_db::sqlx::query(&sql)
            .bind(now)
            .bind(row.id)
            .execute(database.pool())
            .await?;
    }

    if row.token_type == "clr" {
        if config.truthy("cluster.addresses") {
            return Ok(Outcome::Unsupported("cluster API token"));
        }
        return Ok(Outcome::Done(Err(InvalidAuth(
            "This request originates from outside of the cluster address range. Requests \
             signed with cluster API tokens must originate from within the cluster."
                .to_owned(),
        ))));
    }

    if !user.can_establish_api_sessions(config) {
        return Ok(Outcome::Done(Err(InvalidAuth(
            "User account is not permitted to use the API.".to_owned(),
        ))));
    }
    Ok(Outcome::Done(Ok(user)))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("api-abcdefghijklmnopqrstuvwxyz12", None)]
    #[case("cli-abcdefghijklmnopqrstuvwxyz12", None)]
    #[case(
        "api-short",
        Some(
            "API token \"api-short\" has the wrong length. API tokens should be 32 characters long."
        )
    )]
    #[case(
        "xyz-abcdefghijklmnopqrstuvwxyz12",
        Some(
            "API token \"xyz-abcdefghijklmnopqrstuvwxyz12\" has the wrong format. API tokens should be 32 characters long and begin with one of these prefixes: api, cli, clr."
        )
    )]
    fn checks_format(#[case] token: &str, #[case] error: Option<&str>) {
        assert_eq!(
            check_token_format(token).err(),
            error.map(|message| InvalidAuth(message.to_owned()))
        );
    }
}
