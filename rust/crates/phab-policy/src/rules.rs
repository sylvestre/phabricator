//! Policy rules, used both as object policies (`obj.differential.reviewers`)
//! and inside custom policies.

use phab_core::Phid;
use serde_json::Value;

use crate::{ObjectKind, PolicyFacts, PolicySubject, Viewer};

/// The rule classes that can appear in a custom policy. Classes we don't
/// know (or whose application was removed) reject, which is what PHP does for
/// objects they can't apply to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Users,
    Administrators,
    ProjectsAny,
    ProjectsAll,
    LunarPhase,
    ProjectMembers,
    Subscribers,
    DifferentialReviewers,
}

impl Rule {
    pub fn from_class(class: &str) -> Option<Self> {
        let rule = match class {
            "PhabricatorUsersPolicyRule" => Self::Users,
            "PhabricatorAdministratorsPolicyRule" => Self::Administrators,
            "PhabricatorProjectsPolicyRule" => Self::ProjectsAny,
            "PhabricatorProjectsAllPolicyRule" => Self::ProjectsAll,
            "PhabricatorLunarPhasePolicyRule" => Self::LunarPhase,
            "PhabricatorProjectMembersPolicyRule" => Self::ProjectMembers,
            "PhabricatorSubscriptionsSubscribersPolicyRule" => Self::Subscribers,
            "PhabricatorDifferentialReviewersPolicyRule" => Self::DifferentialReviewers,
            _ => return None,
        };
        Some(rule)
    }

    /// The rule behind an `obj.<key>` object policy.
    pub fn from_object_policy_key(key: &str) -> Option<Self> {
        let rule = match key {
            "project.members" => Self::ProjectMembers,
            "subscriptions.subscribers" => Self::Subscribers,
            "differential.reviewers" => Self::DifferentialReviewers,
            _ => return None,
        };
        Some(rule)
    }

    /// `PhabricatorPolicyRule::canApplyToObject()`.
    pub fn can_apply_to(self, subject: &PolicySubject) -> bool {
        match self {
            Self::ProjectMembers => subject.kind == ObjectKind::Project,
            Self::DifferentialReviewers => subject.kind == ObjectKind::Revision,
            Self::Subscribers => subject.subscribable,
            _ => true,
        }
    }

    /// `PhabricatorPolicyRule::applyRule()`.
    pub fn apply(
        self,
        viewer: &Viewer,
        value: &Value,
        subject: &PolicySubject,
        facts: &PolicyFacts,
    ) -> bool {
        let subject_phid = subject.phid.as_ref();
        match self {
            Self::Users => value
                .as_array()
                .is_some_and(|phids| phids.iter().any(|phid| loosely_is_viewer(phid, viewer))),
            Self::Administrators => viewer.is_admin,
            Self::ProjectsAny => value
                .as_array()
                .is_some_and(|phids| phids.iter().any(|phid| is_viewer_project(phid, facts))),
            // An empty list matches, as in PHP.
            Self::ProjectsAll => value
                .as_array()
                .is_some_and(|phids| phids.iter().all(|phid| is_viewer_project(phid, facts))),
            Self::LunarPhase => lunar_phase_matches(facts.now, value),
            Self::ProjectMembers => {
                viewer.is_logged_in()
                    && subject_phid.is_some_and(|phid| facts.viewer_direct_projects.contains(phid))
            }
            Self::Subscribers => {
                let Some(viewer_phid) = viewer.phid.as_ref() else {
                    return false;
                };
                subject.implicit_subscribers.contains(viewer_phid)
                    || subject_phid.is_some_and(|phid| facts.subscribed.contains(phid))
            }
            Self::DifferentialReviewers => {
                viewer.is_logged_in()
                    && subject_phid.is_some_and(|phid| facts.reviewing.contains(phid))
            }
        }
    }
}

fn loosely_is_viewer(value: &Value, viewer: &Viewer) -> bool {
    match (value.as_str(), viewer.phid.as_ref()) {
        (Some(phid), Some(viewer_phid)) => phid == viewer_phid.as_str(),
        _ => false,
    }
}

fn is_viewer_project(value: &Value, facts: &PolicyFacts) -> bool {
    value
        .as_str()
        .is_some_and(|phid| facts.viewer_projects.contains(&Phid::new(phid)))
}

/// `PhutilLunarPhase`.
fn lunar_phase(epoch: i64) -> f64 {
    const NEW_MOON_EPOCH: i64 = 934_354_800;
    const PHASE_LENGTH: f64 = 2_551_442.876_899_2;
    // `%` on floats has the sign of the dividend, like PHP's fmod().
    #[allow(clippy::cast_precision_loss)]
    let elapsed = (epoch - NEW_MOON_EPOCH) as f64;
    (elapsed % PHASE_LENGTH) / PHASE_LENGTH
}

fn lunar_phase_matches(epoch: i64, value: &Value) -> bool {
    let phase = lunar_phase(epoch);
    match value.as_str() {
        Some("full") => (phase - 0.5).abs() < 0.02,
        Some("new") => !(0.02..=0.98).contains(&phase),
        Some("waxing") => phase < 0.5,
        Some("waning") => phase > 0.5,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    // Dates and expectations from PhutilLunarPhaseTestCase.
    #[rstest]
    #[case(934_354_800, [false, true, true, false])]
    #[case(1_116_745_200, [true, false, true, false])]
    #[case(1_116_831_600, [true, false, false, true])]
    #[case(1_117_436_400, [false, false, false, true])]
    fn lunar_phases_match_phutil(#[case] epoch: i64, #[case] expected: [bool; 4]) {
        let actual = ["full", "new", "waxing", "waning"]
            .map(|phase| lunar_phase_matches(epoch, &json!(phase)));
        assert_eq!(actual, expected);
    }

    #[test]
    fn unknown_lunar_value_rejects() {
        assert!(!lunar_phase_matches(934_354_800, &json!("gibbous")));
    }

    #[test]
    fn users_rule_needs_a_list_containing_the_viewer() {
        let subject = PolicySubject::new(None, ObjectKind::Other);
        let facts = PolicyFacts::default();
        let alice = Viewer::user(Phid::new("PHID-USER-alice"), false);
        let value = json!(["PHID-USER-bob", "PHID-USER-alice"]);
        assert!(Rule::Users.apply(&alice, &value, &subject, &facts));
        assert!(!Rule::Users.apply(&Viewer::anonymous(), &value, &subject, &facts));
        assert!(!Rule::Users.apply(&alice, &json!("PHID-USER-alice"), &subject, &facts));
    }

    #[test]
    fn project_rules() {
        let subject = PolicySubject::new(None, ObjectKind::Other);
        let facts = PolicyFacts {
            viewer_projects: [Phid::new("PHID-PROJ-a")].into(),
            ..PolicyFacts::default()
        };
        let viewer = Viewer::user(Phid::new("PHID-USER-alice"), false);
        let both = json!(["PHID-PROJ-a", "PHID-PROJ-b"]);
        assert!(Rule::ProjectsAny.apply(&viewer, &both, &subject, &facts));
        assert!(!Rule::ProjectsAll.apply(&viewer, &both, &subject, &facts));
        assert!(Rule::ProjectsAll.apply(&viewer, &json!(["PHID-PROJ-a"]), &subject, &facts));
        assert!(Rule::ProjectsAll.apply(&viewer, &json!([]), &subject, &facts));
    }
}
