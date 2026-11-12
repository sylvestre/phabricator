//! Objects as the policy filter sees them.

use std::collections::HashMap;

use phab_core::Phid;

use crate::{Capability, Viewer};

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
