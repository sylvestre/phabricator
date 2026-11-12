//! Port of `PhabricatorHash`. Output must stay byte-identical to PHP: these
//! digests are stored in the database (session keys, cache indexes) and both
//! stacks read them.

use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use thiserror::Error;

const INDEX_DIGEST_LENGTH: usize = 12;
const ANCHOR_DIGEST_LENGTH: usize = 12;
const INDEX_MAP: &[u8; 64] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ._";
const ANCHOR_MAP: &[u8; 62] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HashError {
    #[error("HMAC keys must not be empty")]
    EmptyKey,
    #[error("digestToLength() needs a length of at least {minimum}, got {requested}")]
    LengthTooShort { minimum: usize, requested: usize },
}

/// `PhabricatorHash::weakDigest()`: hex HMAC-SHA1. The caller passes the
/// `security.hmac-key` value explicitly.
pub fn weak_digest(message: &[u8], key: &[u8]) -> Result<String, HashError> {
    if key.is_empty() {
        return Err(HashError::EmptyKey);
    }
    let mut mac = Hmac::<Sha1>::new_from_slice(key).map_err(|_| HashError::EmptyKey)?;
    mac.update(message);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// `PhabricatorHash::digestHMACSHA256()`: hex HMAC-SHA256.
pub fn hmac_sha256(message: &[u8], key: &[u8]) -> Result<String, HashError> {
    if key.is_empty() {
        return Err(HashError::EmptyKey);
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| HashError::EmptyKey)?;
    mac.update(message);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// `PhabricatorHash::digestForIndex()`.
pub fn digest_for_index(message: &[u8]) -> String {
    let hash = Sha1::digest(message);
    hash[..INDEX_DIGEST_LENGTH]
        .iter()
        .map(|byte| char::from(INDEX_MAP[usize::from(byte & 0x3f)]))
        .collect()
}

/// `PhabricatorHash::digestForAnchor()`.
pub fn digest_for_anchor(message: &[u8]) -> String {
    let hash = Sha1::digest(message);
    let mut accumulator = 0usize;
    let mut result = String::with_capacity(ANCHOR_DIGEST_LENGTH);
    for &byte in &hash[..ANCHOR_DIGEST_LENGTH] {
        let low_bits = usize::from(byte & 0x3f);
        accumulator = (accumulator + usize::from(byte)) % ANCHOR_MAP.len();
        let index = if low_bits < ANCHOR_MAP.len() {
            low_bits
        } else {
            accumulator
        };
        result.push(char::from(ANCHOR_MAP[index]));
    }
    result
}

/// `PhabricatorHash::digestToLength()`, working on bytes like PHP's `substr()`.
pub fn digest_to_length(message: &[u8], length: usize) -> Result<Vec<u8>, HashError> {
    let minimum = INDEX_DIGEST_LENGTH + 2;
    if length < minimum {
        return Err(HashError::LengthTooShort {
            minimum,
            requested: length,
        });
    }
    let prefix_length = (length - (minimum - 1)).min(message.len());
    let mut result = message[..prefix_length].to_vec();
    result.push(b'-');
    result.extend_from_slice(digest_for_index(message).as_bytes());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    // Expected values come from running the PHP implementation in
    // src/infrastructure/util/PhabricatorHash.php.
    #[rstest]
    #[case("", "qVzKuHbdOl_L", "qVzKuHbdOlPL")]
    #[case("hello", "GQ6ts5Eyq.uf", "GQ6ts5Eyq0uf")]
    #[case("user.profile.image.uri.v1", "y_FPXujT93U3", "yFFPXujT93U3")]
    #[case("session.digest", "1bgT5TDCvwo9", "1bgT5TDCvwo9")]
    fn index_and_anchor_digests_match_php(
        #[case] message: &str,
        #[case] index: &str,
        #[case] anchor: &str,
    ) {
        assert_eq!(digest_for_index(message.as_bytes()), index);
        assert_eq!(digest_for_anchor(message.as_bytes()), anchor);
    }

    #[rstest]
    #[case("", "6e4171878d4a0349b6ecb1b0f6938614515763d2")]
    #[case("hello", "48ecc7cb158b5419864a2a988f4f32561b80b583")]
    #[case("session.digest", "842b1735f31056ef84b9050f7c115598282ddf7c")]
    fn weak_digest_matches_php(#[case] message: &str, #[case] expected: &str) {
        assert_eq!(weak_digest(message.as_bytes(), b"k3y").unwrap(), expected);
    }

    #[rstest]
    #[case(
        "x",
        "5009475861b06a539e2abb93495ecf660dc1ab88c86c6149a98060ae419cdb2d"
    )]
    #[case(
        "hello",
        "876e76604d6817debfcac7bda97f616a5641178941207e9d557beac9e44bc25a"
    )]
    fn hmac_sha256_matches_php(#[case] message: &str, #[case] expected: &str) {
        assert_eq!(hmac_sha256(message.as_bytes(), b"k3y").unwrap(), expected);
    }

    #[test]
    fn empty_keys_are_rejected() {
        assert_eq!(weak_digest(b"x", b""), Err(HashError::EmptyKey));
        assert_eq!(hmac_sha256(b"x", b""), Err(HashError::EmptyKey));
    }

    #[test]
    fn digest_to_length_matches_php() {
        let digest = digest_to_length(b"crabapplepie-and-more-crabapples", 20).unwrap();
        assert_eq!(digest, b"crabapp-DhSlg21TRTqg");
    }

    #[test]
    fn digest_to_length_rejects_short_lengths() {
        assert_eq!(
            digest_to_length(b"abc", 13),
            Err(HashError::LengthTooShort {
                minimum: 14,
                requested: 13
            })
        );
    }
}
