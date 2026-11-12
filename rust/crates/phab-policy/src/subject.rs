//! Objects as the policy filter sees them.

use std::collections::HashMap;

use phab_core::Phid;

use crate::{Capability, Viewer};

/// What a policy rule's `canApplyToObject()` tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// A `DifferentialRevision` (`differential.reviewers`).
    Revision,
    /// A `PhabricatorProject` (`project.members`).
    Project,
    /// Any other object.
    Other,
}

/// Who gets a capability without any policy check
/// (`hasAutomaticCapability()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomaticCapability {
    /// The viewer PHID that gets the capabilities.
    pub principal: Phid,
    /// `None` grants every capability.
    pub capabilities: Option<Vec<Capability>>,
}

/// `PhabricatorExtendedPolicyInterface::getExtendedPolicy()`: to have
/// `for_capability` on the subject, the viewer also needs `capabilities` on
/// `object`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedPolicy {
    /// The capability on the subject this applies to.
    pub for_capability: Capability,
    /// The other object.
    pub object: Phid,
    /// What the viewer needs on `object`.
    pub capabilities: Vec<Capability>,
}

/// Everything the filter needs to know about one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySubject {
    /// `None` for objects without a PHID, which skip the application check.
    pub phid: Option<Phid>,
    /// What object rules can apply to.
    pub kind: ObjectKind,
    /// `getCapabilities()`; testing any other capability is an error.
    pub capabilities: Vec<Capability>,
    /// Policy per capability; a missing or empty policy means "users".
    pub policies: HashMap<Capability, String>,
    /// `hasAutomaticCapability()` grants; they bypass policies but not spaces.
    pub automatic: Vec<AutomaticCapability>,
    /// `Some(space)` for `PhabricatorSpacesInterface` objects, where
    /// `Some(None)` is the default space.
    pub space: Option<Option<Phid>>,
    /// Extended policies, all of which must pass.
    pub extended: Vec<ExtendedPolicy>,
    /// `PhabricatorSubscribableInterface` objects.
    pub subscribable: bool,
    /// `isAutomaticallySubscribed()` holds for these PHIDs.
    pub implicit_subscribers: Vec<Phid>,
}

impl PolicySubject {
    /// A subject supporting view and edit, with no policies (so "users").
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
    /// Sets the policy for `capability`.
    pub fn with_policy(mut self, capability: Capability, policy: impl Into<String>) -> Self {
        self.policies.insert(capability, policy.into());
        self
    }

    /// The policy stored for `capability`, which may be empty.
    pub fn policy(&self, capability: Capability) -> Option<&str> {
        self.policies.get(&capability).map(String::as_str)
    }

    pub(crate) fn has_automatic_capability(&self, capability: Capability, viewer: &Viewer) -> bool {
        self.automatic.iter().any(|automatic| {
            viewer.is(&automatic.principal)
                && automatic
                    .capabilities
                    .as_ref()
                    .is_none_or(|capabilities| capabilities.contains(&capability))
        })
    }
}
