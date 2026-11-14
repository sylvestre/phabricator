//! What the filter knows about the viewer and the objects around a subject.

use std::collections::{HashMap, HashSet};

use phab_core::Phid;

use crate::{ApplicationPolicies, PolicySubject};

/// A `PHID-PLCY` custom policy (`phabricator_policy.policy`).
#[derive(Debug, Clone, PartialEq)]
pub struct CustomPolicy {
    /// The decoded `rules` column: a list of `{action, rule, value}`.
    pub rules: serde_json::Value,
    /// `defaultAction`, used when no rule matches; anything but `allow`
    /// denies.
    pub default_action: String,
    /// The objects the rules were prepared for, when the policy was loaded
    /// by an earlier page of the same query ([`QueryMemory`]); `None` when
    /// they were prepared for the objects being filtered.
    pub prepared_for: Option<HashSet<Phid>>,
}

/// What `PhabricatorPolicyFilter` keeps from one page of a query to the
/// next. `loadCustomPolicies()` prepares the rules of a custom policy for
/// the objects of the page (`willApplyRules()`), then adds the policy with
/// `+=`, which keeps the copy of the first page that loaded it. Rules
/// which read per-object data (subscribers, reviewers, project members)
/// therefore only match the objects of that first page: later pages see
/// fewer objects than the policy allows. PHP's answers depend on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryMemory {
    /// The custom policies loaded so far, with the objects their rules
    /// were prepared for.
    custom_policies: HashMap<Phid, HashSet<Phid>>,
}

impl QueryMemory {
    /// Marks the custom policies an earlier page loaded in `facts`, and
    /// remembers the ones loaded for the first time, with the objects of
    /// `subjects` they apply to for `capability`.
    pub fn apply(
        &mut self,
        facts: &mut PolicyFacts,
        subjects: &[&PolicySubject],
        capability: crate::Capability,
    ) {
        for (policy_phid, policy) in &mut facts.custom_policies {
            if let Some(prepared) = self.custom_policies.get(policy_phid) {
                policy.prepared_for = Some(prepared.clone());
            }
        }
        for subject in subjects {
            let (Some(policy), Some(phid)) = (subject.policy(capability), subject.phid.as_ref())
            else {
                continue;
            };
            let policy = Phid::new(policy);
            if !facts.custom_policies.contains_key(&policy) {
                continue;
            }
            let first_page = facts.custom_policies[&policy].prepared_for.is_none();
            if first_page {
                self.custom_policies
                    .entry(policy)
                    .or_default()
                    .insert(phid.clone());
            }
        }
    }
}

/// A space from `phabricator_spaces.spaces_namespace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    /// The space with its view and edit policies.
    pub subject: PolicySubject,
    /// `isDefaultNamespace`.
    pub is_default: bool,
}

/// Data the policy filter consults, loaded for one viewer.
#[derive(Debug, Clone, Default)]
pub struct PolicyFacts {
    /// `policy.allow-public`; when false, "public" policies mean "users".
    pub allow_public: bool,
    /// Application installation and policies.
    pub applications: ApplicationPolicies,
    /// Projects the viewer is a (materialized) member of.
    pub viewer_projects: HashSet<Phid>,
    /// Projects the viewer is a direct member of (`project.members`).
    pub viewer_direct_projects: HashSet<Phid>,
    /// Custom policies by PHID; a missing one rejects.
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
