//! What the filter knows about the viewer and the objects around a subject.

use std::collections::{HashMap, HashSet};

use phab_core::Phid;

use crate::{ApplicationPolicies, PolicySubject};

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
