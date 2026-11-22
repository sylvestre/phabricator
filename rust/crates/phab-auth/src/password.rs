//! Account passwords, as `PhabricatorAuthPasswordEngine::isValidPassword()`
//! checks them with `PhabricatorBcryptPasswordHasher`.
//!
//! Only what the install produces today is ported: `bcrypt` hashes of the
//! HMAC-SHA256 digest of the password (`PhabricatorUser::newPasswordDigest()`
//! without a legacy format), at the hasher's cost. Anything else (iterated
//! MD5, the `v1` legacy digest, another cost) is [`Outcome::Unsupported`]:
//! PHP would upgrade the hash on a match, through a transaction Rust
//! doesn't write.

use std::fmt;

use phab_core::{Phid, hash};
use phab_db::{Database, rows};

use crate::{AuthError, Outcome};

/// `PhabricatorAuthPassword::PASSWORD_TYPE_ACCOUNT`.
const ACCOUNT_PASSWORD: &str = "account";
/// `PhabricatorBcryptPasswordHasher::getHashName()`.
const BCRYPT: &str = "bcrypt";
/// `PhabricatorBcryptPasswordHasher::getBcryptCost()`.
const BCRYPT_COST: u32 = 11;

/// A password typed by a user, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    /// Wraps a password from a form.
    pub fn new(password: impl Into<String>) -> Self {
        Self(password.into())
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Password(<redacted>)")
    }
}

/// A stored `auth_password` row.
struct StoredPassword {
    hash: String,
    salt: String,
    legacy_digest_format: Option<String>,
}

/// The bcrypt part of `name:hash`, if the hash is a bcrypt one.
fn bcrypt_hash(stored: &str) -> Result<&str, &'static str> {
    // `parseHashFromStorage()`: `list($name, $hash) = explode(':', ...)`.
    let mut parts = stored.split(':');
    let (Some(name), Some(hash)) = (parts.next(), parts.next()) else {
        return Err("a malformed password hash");
    };
    if name != BCRYPT {
        return Err("a password hasher other than bcrypt");
    }
    Ok(hash)
}

/// The cost of a bcrypt hash (`$2y$11$...`).
fn bcrypt_cost(hash: &str) -> Option<u32> {
    hash.split('$').nth(2)?.parse().ok()
}

/// `isValidPassword()` for an account password: whether `password` matches
/// one of the user's passwords which are not revoked.
pub async fn is_valid_account_password(
    database: &Database,
    user: &Phid,
    password: &Password,
) -> Result<Outcome<bool>, AuthError> {
    let sql = format!(
        "SELECT passwordHash, passwordSalt, legacyDigestFormat FROM {} \
         WHERE objectPHID IN (?) AND passwordType IN (?) AND isRevoked = 0 ORDER BY id DESC",
        database.table("auth", "auth_password")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .bind(ACCOUNT_PASSWORD)
        .fetch_all(database.pool())
        .await?;
    let mut stored = Vec::with_capacity(found.len());
    for row in &found {
        stored.push(StoredPassword {
            hash: rows::text(row, "passwordHash")?,
            salt: rows::text(row, "passwordSalt")?,
            legacy_digest_format: rows::opt_text(row, "legacyDigestFormat")?,
        });
    }
    for password_row in &stored {
        if password_row.legacy_digest_format.is_some() {
            return Ok(Outcome::Unsupported("a legacy password digest"));
        }
        if let Err(reason) = bcrypt_hash(&password_row.hash) {
            return Ok(Outcome::Unsupported(reason));
        }
        if password_row.salt.is_empty() {
            return Ok(Outcome::Unsupported("a password without salt"));
        }
    }

    let mut matched = false;
    for password_row in stored {
        // `newPasswordDigest()`: HMAC-SHA256 keyed with the row's salt.
        let digest = hash::hmac_sha256(password.0.as_bytes(), password_row.salt.as_bytes())?;
        let bcrypt = bcrypt_hash(&password_row.hash)
            .unwrap_or_default()
            .to_owned();
        let cost = bcrypt_cost(&bcrypt);
        // Hashing takes a fraction of a second: keep it off the runtime.
        let is_match =
            tokio::task::spawn_blocking(move || bcrypt::verify(digest, &bcrypt).unwrap_or(false))
                .await
                .unwrap_or(false);
        if is_match {
            // `upgradeHashers()` would rehash it at the current cost.
            if cost != Some(BCRYPT_COST) {
                return Ok(Outcome::Unsupported("a bcrypt hash to upgrade"));
            }
            matched = true;
        }
    }
    Ok(Outcome::Done(matched))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("bcrypt:$2y$11$abc", Ok("$2y$11$abc"))]
    #[case("bcrypt:$2y$11$abc:extra", Ok("$2y$11$abc"))]
    #[case("md5:abc", Err("a password hasher other than bcrypt"))]
    #[case("nocolon", Err("a malformed password hash"))]
    fn parses_storage_hashes(#[case] stored: &str, #[case] expected: Result<&str, &str>) {
        assert_eq!(bcrypt_hash(stored), expected);
    }

    #[test]
    fn reads_bcrypt_costs() {
        assert_eq!(bcrypt_cost("$2y$11$abcdef"), Some(11));
        assert_eq!(bcrypt_cost("$2y$10$abcdef"), Some(10));
        assert_eq!(bcrypt_cost("garbage"), None);
    }

    // A hash made by PHP: password_hash(hash_hmac('sha256', 'correct horse',
    // 'saltsaltsalt'), PASSWORD_BCRYPT, ['cost' => 4]).
    #[test]
    fn verifies_php_hashes() {
        let digest = hash::hmac_sha256(b"correct horse", b"saltsaltsalt").unwrap();
        let php_hash = include_str!("../tests/data/php_bcrypt_hash.txt").trim();
        assert!(bcrypt::verify(&digest, php_hash).unwrap());
        let wrong = hash::hmac_sha256(b"wrong horse", b"saltsaltsalt").unwrap();
        assert!(!bcrypt::verify(&wrong, php_hash).unwrap());
    }

    #[test]
    fn passwords_are_not_printed() {
        assert_eq!(format!("{:?}", Password::new("x")), "Password(<redacted>)");
    }
}
