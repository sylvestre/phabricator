//! Port of the Phabricator policy system (`PhabricatorPolicyFilter` and the
//! policy rules it evaluates).
//!
//! **This crate is a security boundary.** A divergence from PHP either hides
//! objects users should see or, worse, leaks private revisions. Changes need
//! a dedicated review.
//!
//! The decision logic in [`filter`] is pure: it works on [`PolicySubject`]
//! descriptions of objects and on [`PolicyFacts`], the data PHP would load
//! while filtering (project memberships, custom policies, spaces...).
//! [`loader`] fills `PolicyFacts` from the database. Missing facts always
//! count against the viewer, so an incomplete load fails closed.

pub mod application;
pub mod filter;
pub mod loader;
pub mod rules;

use std::collections::{HashMap, HashSet};

use phab_core::Phid;

pub use application::ApplicationPolicies;
pub use filter::{PolicyError, filter, has_capability};

pub const POLICY_PUBLIC: &str = "public";
pub const POLICY_USER: &str = "users";
pub const POLICY_ADMIN: &str = "admin";
pub const POLICY_NOONE: &str = "no-one";
pub const OBJECT_POLICY_PREFIX: &str = "obj.";

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

/// The user a policy decision is made for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Viewer {
    /// `None` for logged-out viewers.
    pub phid: Option<Phid>,
    pub is_admin: bool,
    /// `PhabricatorUser::getOmnipotentUser()`, used by internal code paths.
    pub is_omnipotent: bool,
}

impl Viewer {
    pub fn anonymous() -> Self {
        Self::default()
    }

    pub fn user(phid: Phid, is_admin: bool) -> Self {
        Self {
            phid: Some(phid),
            is_admin,
            is_omnipotent: false,
        }
    }

    pub fn omnipotent() -> Self {
        Self {
            phid: None,
            is_admin: false,
            is_omnipotent: true,
        }
    }

    pub fn is_logged_in(&self) -> bool {
        self.phid.is_some()
    }

    fn is(&self, phid: &Phid) -> bool {
        self.phid.as_ref() == Some(phid)
    }
}

/// What a policy rule's `canApplyToObject()` tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Revision,
    Project,
    Other,
}

/// Who gets a capability without any policy check
/// (`hasAutomaticCapability()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomaticCapability {
    pub principal: Phid,
    /// `None` grants every capability.
    pub capabilities: Option<Vec<Capability>>,
}

/// `PhabricatorExtendedPolicyInterface::getExtendedPolicy()`: to have
/// `for_capability` on the subject, the viewer also needs `capabilities` on
/// `object`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedPolicy {
    pub for_capability: Capability,
    pub object: Phid,
    pub capabilities: Vec<Capability>,
}

/// Everything the filter needs to know about one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySubject {
    pub phid: Option<Phid>,
    pub kind: ObjectKind,
    pub capabilities: Vec<Capability>,
    /// Policy per capability; a missing or empty policy means "users".
    pub policies: HashMap<Capability, String>,
    pub automatic: Vec<AutomaticCapability>,
    /// `Some(space)` for `PhabricatorSpacesInterface` objects, where
    /// `Some(None)` is the default space.
    pub space: Option<Option<Phid>>,
    pub extended: Vec<ExtendedPolicy>,
    /// `PhabricatorSubscribableInterface` objects.
    pub subscribable: bool,
    /// `isAutomaticallySubscribed()` holds for these PHIDs.
    pub implicit_subscribers: Vec<Phid>,
}

impl PolicySubject {
    pub fn new(phid: Option<Phid>, kind: ObjectKind) -> Self {
        Self {
            phid,
            kind,
            capabilities: vec![Capability::VIEW, Capability::EDIT],
            policies: HashMap::new(),
            automatic: Vec::new(),
            space: None,
            extended: Vec::new(),
            subscribable: false,
            implicit_subscribers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_policy(mut self, capability: Capability, policy: impl Into<String>) -> Self {
        self.policies.insert(capability, policy.into());
        self
    }

    pub fn policy(&self, capability: Capability) -> Option<&str> {
        self.policies.get(&capability).map(String::as_str)
    }

    fn has_automatic_capability(&self, capability: Capability, viewer: &Viewer) -> bool {
        self.automatic.iter().any(|automatic| {
            viewer.is(&automatic.principal)
                && automatic
                    .capabilities
                    .as_ref()
                    .is_none_or(|capabilities| capabilities.contains(&capability))
        })
    }
}

/// A `PHID-PLCY` custom policy (`phabricator_policy.policy`).
#[derive(Debug, Clone, PartialEq)]
pub struct CustomPolicy {
    /// The decoded `rules` column: a list of `{action, rule, value}`.
    pub rules: serde_json::Value,
    pub default_action: String,
}

/// A space from `phabricator_spaces.spaces_namespace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    pub subject: PolicySubject,
    pub is_default: bool,
}

/// Data the policy filter consults, loaded for one viewer.
#[derive(Debug, Clone, Default)]
pub struct PolicyFacts {
    pub allow_public: bool,
    pub applications: ApplicationPolicies,
    /// Projects the viewer is a (materialized) member of.
    pub viewer_projects: HashSet<Phid>,
    /// Projects the viewer is a direct member of (`project.members`).
    pub viewer_direct_projects: HashSet<Phid>,
    pub custom_policies: HashMap<Phid, CustomPolicy>,
    /// Every space; empty when the install has none.
    pub spaces: HashMap<Phid, Space>,
    /// Revisions where the viewer, or a project they are a member of, is a
    /// reviewer.
    pub reviewing: HashSet<Phid>,
    /// Objects the viewer, or a project they are a member of, is subscribed
    /// to.
    pub subscribed: HashSet<Phid>,
    /// Objects named by extended policies.
    pub extended_objects: HashMap<Phid, PolicySubject>,
    /// Unix time, for the lunar phase rule.
    pub now: i64,
}
