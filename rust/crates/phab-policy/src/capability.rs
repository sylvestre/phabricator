//! Capabilities, like viewing or editing an object.

use std::fmt;

/// A capability key, the value of a `PhabricatorPolicyCapability::CAPABILITY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Capability(pub &'static str);

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Capability {
    /// The capability key.
    pub fn as_str(self) -> &'static str {
        self.0
    }

    /// `PhabricatorPolicyCapability::CAN_VIEW`.
    pub const VIEW: Self = Self("view");
    /// `PhabricatorPolicyCapability::CAN_EDIT`.
    pub const EDIT: Self = Self("edit");
    /// `PhabricatorPolicyCapability::CAN_JOIN`.
    pub const JOIN: Self = Self("join");
    /// `PhabricatorPolicyCapability::CAN_INTERACT`.
    pub const INTERACT: Self = Self("interact");

    /// `shouldAllowPublicPolicySetting()`: whether a "public" policy is
    /// honored for this capability instead of being treated as "users".
    pub fn allows_public_policy(self) -> bool {
        matches!(
            self.0,
            "view"
                | "interact"
                | "differential.default.view"
                | "diffusion.default.view"
                | "project.default.view"
                | "files.default.view"
                | "spaces.default.view"
                | "people.browse"
                | "harbormaster.plan.default.view"
                | "calendar.event.default.view"
                | "passphrase.default.view"
        )
    }
}
