//! PHIDs, the string identifiers of every Phabricator object.

use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::random;

/// The four-character type of a PHID, like `USER` in `PHID-USER-...`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PhidType([u8; 4]);

impl PhidType {
    /// `PhabricatorPHIDConstants::PHID_TYPE_UNKNOWN`, returned for malformed PHIDs.
    pub const UNKNOWN: Self = Self(*b"????");
    /// `PhabricatorPHIDConstants::PHID_TYPE_VOID`.
    pub const VOID: Self = Self(*b"VOID");
    /// `PhabricatorPHIDConstants::PHID_TYPE_STRY`, feed stories.
    pub const STORY: Self = Self(*b"STRY");
    /// `PhabricatorApplicationApplicationPHIDType`.
    pub const APPLICATION: Self = Self(*b"APPS");
    /// `PhabricatorPeopleUserPHIDType`.
    pub const USER: Self = Self(*b"USER");
    /// `PhabricatorProjectProjectPHIDType`.
    pub const PROJECT: Self = Self(*b"PROJ");
    /// `PhabricatorPolicyPHIDTypePolicy`.
    pub const POLICY: Self = Self(*b"PLCY");
    /// `PhabricatorSpacesNamespacePHIDType`.
    pub const SPACE: Self = Self(*b"SPCE");
    /// `DifferentialRevisionPHIDType`.
    pub const REVISION: Self = Self(*b"DREV");
    /// `DifferentialDiffPHIDType`.
    pub const DIFF: Self = Self(*b"DIFF");
    /// `PhabricatorRepositoryRepositoryPHIDType`.
    pub const REPOSITORY: Self = Self(*b"REPO");
    /// `PhabricatorRepositoryCommitPHIDType`.
    pub const COMMIT: Self = Self(*b"CMIT");
    /// `PhabricatorFileFilePHIDType`.
    pub const FILE: Self = Self(*b"FILE");
    /// `HeraldRulePHIDType`.
    pub const HERALD_RULE: Self = Self(*b"HRUL");
    /// `PhabricatorApplicationTransactionTransactionPHIDType`.
    pub const TRANSACTION: Self = Self(*b"XACT");
    /// `PhabricatorPHIDConstants::PHID_TYPE_XCMT`, transaction comments.
    pub const TRANSACTION_COMMENT: Self = Self(*b"XCMT");
    /// `PhabricatorAuthSessionPHIDType`.
    pub const SESSION: Self = Self(*b"SSSN");

    /// Wraps a four-byte type code.
    pub const fn new(code: [u8; 4]) -> Self {
        Self(code)
    }

    /// The type code as a string, or `????` if it is not valid UTF-8.
    pub fn as_str(&self) -> &str {
        // Built either from a constant or from bytes of a `str`, so this only
        // fails if a slice boundary splits a multibyte character.
        std::str::from_utf8(&self.0).unwrap_or("????")
    }
}

impl fmt::Display for PhidType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for PhidType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "PhidType({})", self.as_str())
    }
}

/// Equivalent of `phid_get_type()`. Unlike `Phid::new`, accepts any string.
pub fn phid_type_of(value: &str) -> PhidType {
    let bytes = value.as_bytes();
    if bytes.len() < 10 || !bytes.starts_with(b"PHID-") || bytes[9] != b'-' {
        return PhidType::UNKNOWN;
    }
    let code = [bytes[5], bytes[6], bytes[7], bytes[8]];
    if code.contains(&b'-') || std::str::from_utf8(&code).is_err() {
        return PhidType::UNKNOWN;
    }
    PhidType(code)
}

/// Equivalent of `phid_get_subtype()`, e.g. `DREV` in `PHID-XACT-DREV-...`.
pub fn phid_subtype_of(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    if bytes.len() > 14 && bytes[14] == b'-' {
        return value.get(10..14);
    }
    None
}

/// A Phabricator object identifier such as `PHID-USER-abcdefghijklmnopqrst`.
///
/// Values read from storage are kept verbatim: PHP compares PHIDs as plain
/// strings, so normalizing them here would change behavior.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Phid(String);

impl Phid {
    /// Wraps a PHID string as is, without validating it.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Equivalent of `PhabricatorPHID::generateNewPHID()`.
    pub fn generate(phid_type: PhidType, subtype: Option<PhidType>) -> Self {
        match subtype {
            None => Self(format!(
                "PHID-{phid_type}-{}",
                random::random_characters(20)
            )),
            Some(subtype) => Self(format!(
                "PHID-{phid_type}-{subtype}-{}",
                random::random_characters(15)
            )),
        }
    }

    /// The PHID as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwraps the PHID string.
    pub fn into_string(self) -> String {
        self.0
    }

    /// The type of this PHID; see [`phid_type_of`].
    pub fn phid_type(&self) -> PhidType {
        phid_type_of(&self.0)
    }

    /// The subtype of this PHID; see [`phid_subtype_of`].
    pub fn subtype(&self) -> Option<&str> {
        phid_subtype_of(&self.0)
    }
}

/// Lets maps and sets keyed by `Phid` be queried with a `&str`: the derived
/// `Hash` and `Eq` of the inner `String` agree with those of `str`.
impl Borrow<str> for Phid {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Phid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for Phid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Phid({})", self.0)
    }
}

impl AsRef<str> for Phid {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Phid {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Phid {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("PHID-USER-abcdefghijklmnopqrst", "USER")]
    #[case("PHID-XACT-DREV-abcdefghijklmno", "XACT")]
    #[case("PHID-APPS-PhabricatorFeedApplication", "APPS")]
    #[case("PHID-USE-abc", "????")]
    #[case("PHID-US-R-abc", "????")]
    #[case("PHID-USER", "????")]
    #[case("phid-USER-abc", "????")]
    #[case("users", "????")]
    #[case("obj.differential.reviewers", "????")]
    #[case("", "????")]
    fn extracts_type_like_phid_get_type(#[case] value: &str, #[case] expected: &str) {
        assert_eq!(phid_type_of(value).as_str(), expected);
    }

    #[rstest]
    #[case("PHID-XACT-DREV-abcdefghijklmno", Some("DREV"))]
    #[case("PHID-USER-abcdefghijklmnopqrst", None)]
    #[case("PHID-XACT-DREV-", Some("DREV"))]
    #[case("PHID-XACT-DREV", None)]
    fn extracts_subtype(#[case] value: &str, #[case] expected: Option<&str>) {
        assert_eq!(phid_subtype_of(value), expected);
    }

    #[test]
    fn generates_phids_with_php_shape() {
        let plain = Phid::generate(PhidType::USER, None);
        assert_eq!(plain.as_str().len(), "PHID-USER-".len() + 20);
        assert_eq!(plain.phid_type(), PhidType::USER);
        assert_eq!(plain.subtype(), None);

        let nested = Phid::generate(PhidType::TRANSACTION, Some(PhidType::REVISION));
        assert_eq!(nested.as_str().len(), "PHID-XACT-DREV-".len() + 15);
        assert_eq!(nested.phid_type(), PhidType::TRANSACTION);
        assert_eq!(nested.subtype(), Some("DREV"));
    }
}
