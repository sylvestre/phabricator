//! Capabilities, like viewing or editing an object.

/// A capability key, the value of a `PhabricatorPolicyCapability::CAPABILITY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Capability(pub &'static str);

impl Capability {
    pub const VIEW: Self = Self("view");
    pub const EDIT: Self = Self("edit");
    pub const JOIN: Self = Self("join");
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
