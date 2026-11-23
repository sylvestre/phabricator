//! The arithmetic of `PhabricatorTOTPAuthFactor`: RFC 6238 codes over
//! 30-second timesteps, accepted one timestep early or late.

use std::fmt;

use phab_core::hash;

/// `getTimestepDuration()`.
pub const TIMESTEP_DURATION: i64 = 30;
/// `getTimestepWindowSize()`: codes from one timestep before or after are
/// accepted, for clock skew and typing time.
pub const WINDOW_SIZE: i64 = 1;
/// The base32 alphabet of `base32Decode()`.
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A factor's shared secret, base32 as stored in `factorSecret`. Never
/// printed.
#[derive(Clone, PartialEq, Eq)]
pub struct TotpSecret(String);

impl fmt::Debug for TotpSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TotpSecret(<redacted>)")
    }
}

impl TotpSecret {
    /// The secret of a factor, if `base32Decode()` reads it without
    /// warnings: PHP turns the warning of a character outside the alphabet
    /// into an exception.
    pub fn new(secret: impl Into<String>) -> Option<Self> {
        let secret = secret.into();
        secret
            .bytes()
            .all(|byte| BASE32_ALPHABET.contains(&byte.to_ascii_uppercase()))
            .then_some(Self(secret))
    }

    /// `base32Decode()`: whole bytes only, trailing bits dropped.
    fn key(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.0.len() * 5 / 8);
        // PHP keeps every bit in an integer which overflows; only the low
        // 13 bits are ever read.
        let mut accumulator: u32 = 0;
        let mut bits = 0;
        for byte in self.0.bytes() {
            let value = BASE32_ALPHABET
                .iter()
                .position(|known| *known == byte.to_ascii_uppercase())
                .and_then(|position| u32::try_from(position).ok())
                .unwrap_or_default();
            accumulator = ((accumulator << 5) | value) & 0xffff;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out.push(u8::try_from((accumulator >> bits) & 0xff).unwrap_or_default());
            }
        }
        out
    }

    /// `getTOTPCode()`: the six digits for `timestep`. PHP packs the
    /// timestep as a 32-bit big-endian integer after four zero bytes.
    pub fn code(&self, timestep: i64) -> String {
        let mut message = [0u8; 8];
        // `pack('N', ...)` keeps the low 32 bits.
        let low_bits = u32::try_from(timestep.rem_euclid(1 << 32)).unwrap_or_default();
        message[4..].copy_from_slice(&low_bits.to_be_bytes());
        let digest = hash::hmac_sha1_raw(&message, &self.key());
        let offset = usize::from(digest[19] & 0x0f);
        let value = (u32::from(digest[offset] & 0x7f) << 24)
            | (u32::from(digest[offset + 1]) << 16)
            | (u32::from(digest[offset + 2]) << 8)
            | u32::from(digest[offset + 3]);
        format!("{:06}", value % 1_000_000)
    }
}

/// `getCurrentTimestep()`.
pub fn timestep(now: i64) -> i64 {
    now.div_euclid(TIMESTEP_DURATION)
}

/// `getAllowedTimesteps()`: the timesteps around `at`, in order.
pub fn allowed_timesteps(at: i64) -> std::ops::RangeInclusive<i64> {
    (at - WINDOW_SIZE)..=(at + WINDOW_SIZE)
}

/// Whether two windows share a timestep (`isset($allowed[$step])`).
pub fn window_contains(at: i64, timestep: i64) -> bool {
    allowed_timesteps(at).contains(&timestep)
}

/// `getTimestepAtWhichResponseIsValid()` for the timesteps valid both for
/// the challenge issued at `challenge_step` and for `current_step`: the
/// first one whose code is `code`, compared in constant time. Every
/// timestep is computed and compared, so the time taken doesn't say which
/// one matched.
pub fn valid_timestep(
    secret: &TotpSecret,
    challenge_step: i64,
    current_step: i64,
    code: &str,
) -> Option<i64> {
    let mut found = None;
    for timestep in allowed_timesteps(challenge_step) {
        if !window_contains(current_step, timestep) {
            continue;
        }
        let expected = secret.code(timestep);
        if hash::hashes_are_identical(code.as_bytes(), expected.as_bytes()) && found.is_none() {
            found = Some(timestep);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// The RFC 6238 test secret, `12345678901234567890`, in base32.
    const RFC_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    // Expected codes come from `PhabricatorTOTPAuthFactor::getTOTPCode()`
    // run by PHP 8.4 (and match RFC 6238's SHA-1 vectors).
    #[rstest]
    #[case(RFC_SECRET, 1, "287082")]
    #[case(RFC_SECRET, 37_037_036, "081804")]
    #[case(RFC_SECRET, 37_037_037, "050471")]
    #[case(RFC_SECRET, 41_152_263, "005924")]
    #[case(RFC_SECRET, 66_666_666, "279037")]
    #[case(RFC_SECRET, 666_666_666, "353130")]
    #[case("abcdefghijklmnopqrstuvwxyz234567", 56_666_666, "532659")]
    #[case("abcdefghijklmnopqrstuvwxyz234567", 56_666_667, "479687")]
    #[case("abcdefghijklmnopqrstuvwxyz234567", 0, "081962")]
    fn codes_match_php(#[case] secret: &str, #[case] timestep: i64, #[case] expected: &str) {
        assert_eq!(TotpSecret::new(secret).unwrap().code(timestep), expected);
    }

    #[test]
    fn decodes_base32_like_php() {
        assert_eq!(TotpSecret::new("MZXW6").unwrap().key(), b"foo");
        assert_eq!(TotpSecret::new("mzxw6ytboi").unwrap().key(), b"foobar");
        assert!(TotpSecret::new("MZXW1").is_none());
        assert!(TotpSecret::new("MZ=W6").is_none());
    }

    #[test]
    fn responses_must_fit_both_windows() {
        let secret = TotpSecret::new(RFC_SECRET).unwrap();
        let step = 56_666_666;
        assert_eq!(
            valid_timestep(&secret, step, step, &secret.code(step + 1)),
            Some(step + 1)
        );
        assert_eq!(
            valid_timestep(&secret, step, step, &secret.code(step - 1)),
            Some(step - 1)
        );
        assert_eq!(
            valid_timestep(&secret, step, step, &secret.code(step + 2)),
            None
        );
        // A challenge issued one step ago accepts only the overlap.
        assert_eq!(
            valid_timestep(&secret, step - 1, step, &secret.code(step - 2)),
            None
        );
        assert_eq!(
            valid_timestep(&secret, step - 1, step, &secret.code(step)),
            Some(step)
        );
        assert_eq!(valid_timestep(&secret, step, step, ""), None);
        assert_eq!(valid_timestep(&secret, step, step, "12345"), None);
    }

    #[test]
    fn secrets_are_not_printed() {
        assert_eq!(
            format!("{:?}", TotpSecret::new(RFC_SECRET).unwrap()),
            "TotpSecret(<redacted>)"
        );
    }
}
