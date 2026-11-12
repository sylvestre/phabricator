//! The global policies and the prefix of object policies.

/// `PhabricatorPolicies::POLICY_PUBLIC`: anyone, logged in or not, when
/// `policy.allow-public` is set and the capability allows it.
pub const POLICY_PUBLIC: &str = "public";
/// `PhabricatorPolicies::POLICY_USER`: any logged-in user.
pub const POLICY_USER: &str = "users";
/// `PhabricatorPolicies::POLICY_ADMIN`: administrators only.
pub const POLICY_ADMIN: &str = "admin";
/// `PhabricatorPolicies::POLICY_NOONE`: nobody but the omnipotent viewer
/// and automatic capabilities.
pub const POLICY_NOONE: &str = "no-one";
/// `PhabricatorPolicyQuery::OBJECT_POLICY_PREFIX`, the prefix of object
/// policies like `obj.differential.reviewers`.
pub const OBJECT_POLICY_PREFIX: &str = "obj.";
