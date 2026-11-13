//! CSRF tokens, as made and checked by `PhabricatorAuthCSRFEngine` for
//! `PhabricatorUser::getCSRFToken()` and `validateCSRFToken()`.
//!
//! Tokens are shared with PHP: a form rendered by one stack can be posted to
//! the other, and the Rust UI posts to PHP's Conduit API as the logged-in
//! user. A token is `B@` (the BREACH prefix), an 8 character salt and the
//! first 16 hex characters of an HMAC of the secret, the hour and the salt.

use std::fmt;

use phab_core::{Phid, hash, random};
use phab_db::Database;

use crate::{AuthError, Outcome, session};

const BREACH_PREFIX: &str = "B@";
const SALT_LENGTH: usize = 8;
const TOKEN_LENGTH: usize = 16;
const CYCLE_FREQUENCY: i64 = 60 * 60;
/// Tokens from the six previous hours and the next one are accepted.
const WINDOW_OFFSETS: std::ops::RangeInclusive<i64> = -6..=1;
const CSRF_KEY: &str = "csrf";

/// What a user's tokens are made from: the user PHID, the account secret
/// and the session key digest (`PhabricatorUser::newCSRFEngine()`).
#[derive(Clone, PartialEq, Eq)]
pub struct CsrfSecret(String);

impl CsrfSecret {
    /// The secret of a logged-in user with a session.
    pub fn for_session(user_phid: &Phid, account_secret: &str, session_key: &str) -> Self {
        Self(format!(
            "{}{account_secret}{session_key}",
            user_phid.as_str()
        ))
    }
}

impl fmt::Debug for CsrfSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CsrfSecret(<redacted>)")
    }
}

/// The `csrf` HMAC key, which PHP creates the first time it needs it.
#[derive(Clone, PartialEq, Eq)]
pub struct CsrfKey(Vec<u8>);

impl CsrfKey {
    /// A key from its raw bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for CsrfKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CsrfKey(<redacted>)")
    }
}

/// Loads the `csrf` key. Until PHP has created it, Rust can't make tokens
/// PHP accepts, so the request goes to PHP.
pub async fn load_key(database: &Database) -> Result<Outcome<CsrfKey>, AuthError> {
    Ok(match session::named_hmac_key(database, CSRF_KEY).await? {
        Some(key) => Outcome::Done(CsrfKey(key)),
        None => Outcome::Unsupported("the CSRF key does not exist yet"),
    })
}

/// `PhabricatorAuthCSRFEngine::newRawToken()`.
fn raw_token(
    secret: &CsrfSecret,
    key: &CsrfKey,
    salt: &str,
    now: i64,
    offset: i64,
) -> Result<String, AuthError> {
    let time_block = now.div_euclid(CYCLE_FREQUENCY) + offset;
    let message = format!("{}{time_block}{salt}", secret.0);
    let mut digest = hash::hmac_sha256(message.as_bytes(), &key.0)?;
    digest.truncate(TOKEN_LENGTH);
    Ok(digest)
}

/// `PhabricatorAuthCSRFEngine::newToken()`, with a new salt.
pub fn new_token(secret: &CsrfSecret, key: &CsrfKey, now: i64) -> Result<String, AuthError> {
    token_with_salt(secret, key, &random::random_characters(SALT_LENGTH), now)
}

/// `PhabricatorAuthCSRFEngine::newToken()` with a given salt.
pub fn token_with_salt(
    secret: &CsrfSecret,
    key: &CsrfKey,
    salt: &str,
    now: i64,
) -> Result<String, AuthError> {
    let token = raw_token(secret, key, salt, now, 0)?;
    Ok(format!("{BREACH_PREFIX}{salt}{token}"))
}

/// `PhabricatorAuthCSRFEngine::isValidToken()`.
pub fn is_valid_token(
    secret: &CsrfSecret,
    key: &CsrfKey,
    token: &str,
    now: i64,
) -> Result<bool, AuthError> {
    let Some(rest) = token.strip_prefix(BREACH_PREFIX) else {
        return Ok(false);
    };
    // PHP's substr() cuts bytes; a token that is not ASCII can't match.
    if !rest.is_ascii() {
        return Ok(false);
    }
    let (salt, digest) = rest.split_at(rest.len().min(SALT_LENGTH));
    for offset in WINDOW_OFFSETS {
        let expected = raw_token(secret, key, salt, now, offset)?;
        if hash::hashes_are_identical(expected.as_bytes(), digest.as_bytes()) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn secret() -> CsrfSecret {
        CsrfSecret::for_session(&Phid::new("PHID-USER-alice"), "SECRET", "sessdigest")
    }

    fn key() -> CsrfKey {
        CsrfKey::new(b"k3y".to_vec())
    }

    // Expected values come from PHP: substr(hash_hmac('sha256',
    // 'PHID-USER-aliceSECRETsessdigest'.$block.'abcdefgh', 'k3y'), 0, 16).
    #[test]
    fn tokens_match_php() {
        assert_eq!(
            token_with_salt(&secret(), &key(), "abcdefgh", NOW).unwrap(),
            "B@abcdefgh98e019a8e3f3fcf4"
        );
    }

    #[rstest]
    #[case("B@abcdefgh98e019a8e3f3fcf4", NOW, true)]
    // Made six hours earlier, or one hour later.
    #[case("B@abcdefgh6904900c65a39ff1", NOW, true)]
    #[case("B@abcdefghc0357509c71b6499", NOW, true)]
    #[case("B@abcdefgh98e019a8e3f3fcf4", NOW + 7 * 3600, false)]
    #[case("B@abcdefgh98e019a8e3f3fcf4", NOW - 2 * 3600, false)]
    #[case("abcdefgh98e019a8e3f3fcf4", NOW, false)]
    #[case("B@abcdefgh98e019a8e3f3fcf", NOW, false)]
    #[case("B@abcdefgi98e019a8e3f3fcf4", NOW, false)]
    #[case("B@", NOW, false)]
    #[case("B@é", NOW, false)]
    #[case("", NOW, false)]
    fn validation(#[case] token: &str, #[case] now: i64, #[case] expected: bool) {
        assert_eq!(
            is_valid_token(&secret(), &key(), token, now).unwrap(),
            expected
        );
    }

    #[test]
    fn new_tokens_validate() {
        let token = new_token(&secret(), &key(), NOW).unwrap();
        assert_eq!(token.len(), 2 + SALT_LENGTH + TOKEN_LENGTH);
        assert!(is_valid_token(&secret(), &key(), &token, NOW).unwrap());
        let other = CsrfSecret::for_session(&Phid::new("PHID-USER-bob"), "SECRET", "sessdigest");
        assert!(!is_valid_token(&other, &key(), &token, NOW).unwrap());
    }

    #[test]
    fn secrets_are_not_printed() {
        assert_eq!(format!("{:?}", secret()), "CsrfSecret(<redacted>)");
        assert_eq!(format!("{:?}", key()), "CsrfKey(<redacted>)");
    }
}
