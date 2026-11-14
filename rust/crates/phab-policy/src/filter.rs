//! Port of `PhabricatorPolicyFilter::apply()`.

use std::collections::HashMap;

use phab_core::{Phid, PhidType, phid};

use crate::rules::Rule;
use crate::{
    Capability, OBJECT_POLICY_PREFIX, POLICY_ADMIN, POLICY_NOONE, POLICY_PUBLIC, POLICY_USER,
    PolicyFacts, PolicySubject, Viewer, application,
};
use serde_json::Value;
use thiserror::Error;

/// PHP arrests extended policy chains deeper than this, which is how cycles
/// are rejected.
const MAX_EXTENDED_DEPTH: usize = 32;

/// A policy check that PHP rejects with an exception.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    /// A capability was tested on a subject whose `capabilities` lack it.
    #[error(
        "testing for capability '{capability}' on an object ({object}) which does not support it"
    )]
    UnsupportedCapability {
        /// The capability key.
        capability: String,
        /// The subject's PHID, or `<no phid>`.
        object: String,
    },
}

/// Which of `subjects` the viewer has every one of `capabilities` on, in
/// order (`PhabricatorPolicyFilter::apply()`).
///
/// # Errors
///
/// [`PolicyError::UnsupportedCapability`] when a subject that passes its
/// application check lacks one of `capabilities`.
pub fn filter(
    viewer: &Viewer,
    subjects: &[&PolicySubject],
    capabilities: &[Capability],
    facts: &PolicyFacts,
) -> Result<Vec<bool>, PolicyError> {
    Filter { viewer, facts }.apply(subjects, capabilities, 0)
}

/// `PhabricatorPolicyFilter::hasCapability()`.
///
/// # Errors
///
/// As [`filter`].
pub fn has_capability(
    viewer: &Viewer,
    subject: &PolicySubject,
    capability: Capability,
    facts: &PolicyFacts,
) -> Result<bool, PolicyError> {
    Ok(filter(viewer, &[subject], &[capability], facts)?[0])
}

/// `PhabricatorApplication::isClassInstalledForViewer()`.
///
/// # Errors
///
/// As [`filter`].
pub fn can_see_application(
    viewer: &Viewer,
    class: &str,
    facts: &PolicyFacts,
) -> Result<bool, PolicyError> {
    Filter { viewer, facts }.can_see_application(class, 0)
}

struct Filter<'a> {
    viewer: &'a Viewer,
    facts: &'a PolicyFacts,
}

struct ExtendedCheck {
    index: usize,
    object: Phid,
    capabilities: Vec<Capability>,
}

impl Filter<'_> {
    fn apply(
        &self,
        subjects: &[&PolicySubject],
        capabilities: &[Capability],
        depth: usize,
    ) -> Result<Vec<bool>, PolicyError> {
        if self.viewer.is_omnipotent {
            return Ok(vec![true; subjects.len()]);
        }

        let mut visible = Vec::with_capacity(subjects.len());
        for subject in subjects {
            visible.push(self.passes_application_check(subject, depth)?);
        }

        for (subject, _) in subjects.iter().zip(&visible).filter(|(_, ok)| **ok) {
            for capability in capabilities {
                if !subject.capabilities.contains(capability) {
                    return Err(PolicyError::UnsupportedCapability {
                        capability: capability.0.to_owned(),
                        object: subject
                            .phid
                            .as_ref()
                            .map_or_else(|| "<no phid>".to_owned(), ToString::to_string),
                    });
                }
            }
        }

        for (subject, ok) in subjects.iter().zip(visible.iter_mut()) {
            if *ok {
                for capability in capabilities {
                    if !self.check_capability(subject, *capability, depth)? {
                        *ok = false;
                        break;
                    }
                }
            }
        }

        self.apply_extended_checks(subjects, capabilities, &mut visible, depth)?;
        Ok(visible)
    }

    fn passes_application_check(
        &self,
        subject: &PolicySubject,
        depth: usize,
    ) -> Result<bool, PolicyError> {
        let Some(phid) = &subject.phid else {
            return Ok(true);
        };
        match application::application_for_phid_type(phid.phid_type()) {
            Some(class) => self.can_see_application(class, depth),
            None => Ok(true),
        }
    }

    fn can_see_application(&self, class: &str, depth: usize) -> Result<bool, PolicyError> {
        if self.viewer.is_omnipotent {
            return Ok(true);
        }
        let applications = &self.facts.applications;
        if !applications.is_installed(class) {
            return Ok(false);
        }
        if !application::can_uninstall(class) {
            return Ok(true);
        }
        let subject = applications.subject(class, self.facts.allow_public);
        Ok(self.apply(&[&subject], &[Capability::VIEW], depth)?[0])
    }

    fn check_capability(
        &self,
        subject: &PolicySubject,
        capability: Capability,
        depth: usize,
    ) -> Result<bool, PolicyError> {
        let viewer = self.viewer;
        let mut policy = subject
            .policy(capability)
            .filter(|policy| !policy.is_empty())
            .unwrap_or(POLICY_USER);

        if policy == POLICY_PUBLIC
            && (!self.facts.allow_public || !capability.allows_public_policy())
        {
            policy = POLICY_USER;
        }

        if viewer.is_omnipotent {
            return Ok(true);
        }

        if let Some(space) = &subject.space
            && !self.can_see_space(space.as_ref(), depth)?
        {
            return Ok(false);
        }

        if subject.has_automatic_capability(capability, viewer) {
            return Ok(true);
        }

        let allowed = match policy {
            POLICY_PUBLIC => true,
            POLICY_USER => viewer.is_logged_in(),
            POLICY_ADMIN => viewer.is_admin,
            POLICY_NOONE => false,
            _ => {
                if let Some(key) = policy.strip_prefix(OBJECT_POLICY_PREFIX) {
                    self.check_object_policy(key, subject)
                } else {
                    match phid::phid_type_of(policy) {
                        PhidType::PROJECT => self.facts.viewer_projects.contains(policy),
                        PhidType::USER => viewer
                            .phid
                            .as_ref()
                            .is_some_and(|phid| phid.as_str() == policy),
                        PhidType::POLICY => self.check_custom_policy(policy, subject),
                        // Unknown policies reject everyone but the omnipotent
                        // viewer.
                        _ => false,
                    }
                }
            }
        };
        Ok(allowed)
    }

    fn can_see_space(&self, space: Option<&Phid>, depth: usize) -> Result<bool, PolicyError> {
        let spaces = &self.facts.spaces;
        // Without spaces, everything lives in an implicit default space.
        if spaces.is_empty() {
            return Ok(space.is_none());
        }
        let found = match space {
            Some(phid) => spaces.get(phid),
            None => spaces.values().find(|space| space.is_default),
        };
        let Some(found) = found else {
            return Ok(false);
        };
        Ok(self.apply(&[&found.subject], &[Capability::VIEW], depth)?[0])
    }

    fn check_object_policy(&self, key: &str, subject: &PolicySubject) -> bool {
        Rule::from_object_policy_key(key).is_some_and(|rule| {
            rule.can_apply_to(subject) && rule.apply(self.viewer, &Value::Null, subject, self.facts)
        })
    }

    fn check_custom_policy(&self, policy_phid: &str, subject: &PolicySubject) -> bool {
        let Some(policy) = self.facts.custom_policies.get(policy_phid) else {
            return false;
        };
        let rules = policy.rules.as_array().map_or(&[][..], Vec::as_slice);
        let mut action: Option<&str> = None;
        for rule in rules {
            let Some(rule) = rule.as_object() else {
                return false;
            };
            let Some(kind) = rule
                .get("rule")
                .and_then(Value::as_str)
                .and_then(Rule::from_class)
            else {
                return false;
            };
            if !kind.can_apply_to(subject) {
                return false;
            }
            let value = rule.get("value").unwrap_or(&Value::Null);
            // A rule prepared for other objects knows nothing of this one.
            let unprepared = kind.reads_object_data()
                && policy.prepared_for.as_ref().is_some_and(|prepared| {
                    subject
                        .phid
                        .as_ref()
                        .is_none_or(|phid| !prepared.contains(phid))
                });
            let matches = if unprepared {
                kind.apply_unprepared(self.viewer, subject)
            } else {
                kind.apply(self.viewer, value, subject, self.facts)
            };
            if matches {
                action = rule.get("action").and_then(Value::as_str);
                break;
            }
        }
        action.unwrap_or(&policy.default_action) == "allow"
    }

    fn apply_extended_checks(
        &self,
        subjects: &[&PolicySubject],
        capabilities: &[Capability],
        visible: &mut [bool],
        depth: usize,
    ) -> Result<(), PolicyError> {
        let mut checks = Vec::new();
        for (index, subject) in subjects.iter().enumerate() {
            if !visible[index] {
                continue;
            }
            for capability in capabilities {
                let policies = subject
                    .extended
                    .iter()
                    .filter(|extended| extended.for_capability == *capability);
                for extended in policies {
                    let mut needed = extended.capabilities.clone();
                    needed.sort_unstable_by_key(|capability| capability.0);
                    needed.dedup();
                    checks.push(ExtendedCheck {
                        index,
                        object: extended.object.clone(),
                        capabilities: needed,
                    });
                }
            }
        }

        // Group checks needing the same capabilities, in order of first
        // appearance, like PHP's igroup().
        let mut groups: Vec<(Vec<Capability>, Vec<&ExtendedCheck>)> = Vec::new();
        for check in &checks {
            match groups
                .iter_mut()
                .find(|(needed, _)| *needed == check.capabilities)
            {
                Some((_, group)) => group.push(check),
                None => groups.push((check.capabilities.clone(), vec![check])),
            }
        }

        for (needed, group) in groups {
            let mut key_map: HashMap<&Phid, Vec<usize>> = HashMap::new();
            let mut objects: Vec<(&Phid, &PolicySubject)> = Vec::new();
            for check in group {
                if !visible[check.index] {
                    continue;
                }
                let Some(object) = self.facts.extended_objects.get(&check.object) else {
                    // The object an extended policy needs could not be loaded.
                    visible[check.index] = false;
                    continue;
                };
                key_map.entry(&check.object).or_default().push(check.index);
                if !objects.iter().any(|(phid, _)| *phid == &check.object) {
                    objects.push((&check.object, object));
                }
            }
            if objects.is_empty() {
                continue;
            }

            let passed = if depth + 1 > MAX_EXTENDED_DEPTH {
                vec![false; objects.len()]
            } else {
                let refs: Vec<&PolicySubject> = objects.iter().map(|(_, object)| *object).collect();
                self.apply(&refs, &needed, depth + 1)?
            };
            for ((phid, _), ok) in objects.iter().zip(passed) {
                if !ok {
                    for index in &key_map[phid] {
                        visible[*index] = false;
                    }
                }
            }
        }
        Ok(())
    }
}
