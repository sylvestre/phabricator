//! Behavior of the policy filter. The first group ports
//! `PhabricatorPolicyTestCase`; the rest covers the rules and data paths that
//! test case does not reach.

use std::collections::HashMap;

use phab_core::{Config, Phid};
use phab_policy::{
    ApplicationPolicies, AutomaticCapability, Capability, CustomPolicy, ExtendedPolicy, ObjectKind,
    PolicyError, PolicyFacts, PolicySubject, Space, Viewer, filter, filter::can_see_application,
    has_capability,
};
use rstest::rstest;
use serde_json::{Value, json};

fn object(policy: &str) -> PolicySubject {
    PolicySubject::new(None, ObjectKind::Other).with_policy(Capability::VIEW, policy)
}

fn object_with_phid(phid: &str, policy: &str) -> PolicySubject {
    PolicySubject::new(Some(Phid::new(phid)), ObjectKind::Other)
        .with_policy(Capability::VIEW, policy)
}

fn viewer(spec: &str) -> Viewer {
    match spec {
        "public" => Viewer::anonymous(),
        "user" => Viewer::user(Phid::new("PHID-USER-user"), false),
        "admin" => Viewer::user(Phid::new("PHID-USER-admin"), true),
        "omnipotent" => Viewer::omnipotent(),
        _ => unreachable!("test viewers are listed above"),
    }
}

fn can_view(spec: &str, subject: &PolicySubject, facts: &PolicyFacts) -> bool {
    has_capability(&viewer(spec), subject, Capability::VIEW, facts).unwrap()
}

fn visibility(subject: &PolicySubject, facts: &PolicyFacts) -> [bool; 3] {
    ["public", "user", "admin"].map(|spec| can_view(spec, subject, facts))
}

#[rstest]
#[case("public", true, [true, true, true])]
#[case("public", false, [false, true, true])]
#[case("users", false, [false, true, true])]
#[case("admin", false, [false, false, true])]
#[case("no-one", false, [false, false, false])]
#[case("the duck goes quack", false, [false, false, false])]
#[case("", false, [false, true, true])]
fn global_policies(#[case] policy: &str, #[case] allow_public: bool, #[case] expected: [bool; 3]) {
    let facts = PolicyFacts {
        allow_public,
        ..PolicyFacts::default()
    };
    assert_eq!(visibility(&object(policy), &facts), expected);
}

#[test]
fn public_policy_is_users_for_capabilities_which_disallow_public() {
    let facts = PolicyFacts {
        allow_public: true,
        ..PolicyFacts::default()
    };
    let subject = object("users").with_policy(Capability::EDIT, "public");
    let result = has_capability(&viewer("public"), &subject, Capability::EDIT, &facts).unwrap();
    assert!(!result);
}

#[test]
fn omnipotent_viewer_sees_everything() {
    let facts = PolicyFacts::default();
    assert!(can_view("omnipotent", &object("no-one"), &facts));
    assert!(can_view("omnipotent", &object("the cow goes moo"), &facts));
}

#[test]
fn extended_policies() {
    let restrictive = object_with_phid("PHID-TEST-2", "admin");
    let mut subject = object_with_phid("PHID-TEST-1", "users");
    let mut facts = PolicyFacts::default();
    facts
        .extended_objects
        .insert(Phid::new("PHID-TEST-2"), restrictive.clone());
    assert_eq!(visibility(&subject, &facts), [false, true, true]);

    subject.extended = vec![ExtendedPolicy {
        for_capability: Capability::VIEW,
        object: Phid::new("PHID-TEST-2"),
        capabilities: vec![Capability::VIEW],
    }];
    assert_eq!(visibility(&subject, &facts), [false, false, true]);

    subject.extended[0].capabilities = vec![Capability::EDIT];
    let mut edit_only = restrictive.with_policy(Capability::EDIT, "no-one");
    edit_only.capabilities = vec![Capability::EDIT];
    facts
        .extended_objects
        .insert(Phid::new("PHID-TEST-2"), edit_only);
    assert_eq!(visibility(&subject, &facts), [false, false, false]);
}

#[test]
fn extended_policy_cycles_are_rejected() {
    let mut subject = object_with_phid("PHID-TEST-1", "users");
    subject.extended = vec![ExtendedPolicy {
        for_capability: Capability::VIEW,
        object: Phid::new("PHID-TEST-1"),
        capabilities: vec![Capability::VIEW],
    }];
    let mut facts = PolicyFacts::default();
    facts
        .extended_objects
        .insert(Phid::new("PHID-TEST-1"), subject.clone());
    assert_eq!(visibility(&subject, &facts), [false, false, false]);
}

#[test]
fn bulk_extended_policies_reject_every_dependent_object() {
    let target = Phid::new("PHID-TEST-999");
    let make = |phid: &str, capabilities: Vec<Capability>| {
        let mut subject = object_with_phid(phid, "users");
        subject.extended = vec![ExtendedPolicy {
            for_capability: Capability::VIEW,
            object: target.clone(),
            capabilities,
        }];
        subject
    };
    let both = vec![Capability::VIEW, Capability::EDIT];
    let objects = [
        make("PHID-TEST-1", both.clone()),
        make("PHID-TEST-2", vec![Capability::VIEW]),
        make("PHID-TEST-3", both),
    ];
    let mut facts = PolicyFacts::default();
    facts
        .extended_objects
        .insert(target.clone(), object_with_phid("PHID-TEST-999", "admin"));
    let refs: Vec<&PolicySubject> = objects.iter().collect();
    let result = filter(&viewer("user"), &refs, &[Capability::VIEW], &facts).unwrap();
    assert_eq!(result, [false, false, false]);
}

#[test]
fn missing_extended_object_rejects() {
    let mut subject = object_with_phid("PHID-TEST-1", "users");
    subject.extended = vec![ExtendedPolicy {
        for_capability: Capability::VIEW,
        object: Phid::new("PHID-REPO-gone"),
        capabilities: vec![Capability::VIEW],
    }];
    assert!(!can_view("user", &subject, &PolicyFacts::default()));
    assert!(can_view("omnipotent", &subject, &PolicyFacts::default()));
}

#[test]
fn extended_policies_only_apply_to_their_capability() {
    let mut subject = object("users").with_policy(Capability::EDIT, "users");
    subject.extended = vec![ExtendedPolicy {
        for_capability: Capability::EDIT,
        object: Phid::new("PHID-REPO-gone"),
        capabilities: vec![Capability::VIEW],
    }];
    let facts = PolicyFacts::default();
    assert!(can_view("user", &subject, &facts));
    assert!(!has_capability(&viewer("user"), &subject, Capability::EDIT, &facts).unwrap());
}

#[test]
fn every_required_capability_must_pass() {
    let subject = object("users").with_policy(Capability::EDIT, "no-one");
    let result = filter(
        &viewer("user"),
        &[&subject],
        &[Capability::VIEW, Capability::EDIT],
        &PolicyFacts::default(),
    )
    .unwrap();
    assert_eq!(result, [false]);
}

#[test]
fn unsupported_capabilities_are_an_error() {
    let subject = object("users");
    let result = has_capability(
        &viewer("user"),
        &subject,
        Capability::JOIN,
        &PolicyFacts::default(),
    );
    assert!(matches!(
        result,
        Err(PolicyError::UnsupportedCapability { .. })
    ));
}

#[test]
fn automatic_capabilities_bypass_policies() {
    let mut subject = object("no-one").with_policy(Capability::EDIT, "no-one");
    subject.automatic = vec![AutomaticCapability {
        principal: Phid::new("PHID-USER-user"),
        capabilities: Some(vec![Capability::VIEW]),
    }];
    let facts = PolicyFacts::default();
    assert!(can_view("user", &subject, &facts));
    assert!(!can_view("admin", &subject, &facts));
    assert!(!has_capability(&viewer("user"), &subject, Capability::EDIT, &facts).unwrap());
}

#[test]
fn user_and_project_policies() {
    let facts = PolicyFacts {
        viewer_projects: [Phid::new("PHID-PROJ-team")].into(),
        ..PolicyFacts::default()
    };
    assert_eq!(
        visibility(&object("PHID-USER-user"), &facts),
        [false, true, false]
    );
    assert!(can_view("user", &object("PHID-PROJ-team"), &facts));
    assert!(!can_view("user", &object("PHID-PROJ-other"), &facts));
}

fn application_facts(values: Value) -> PolicyFacts {
    let mut config = Config::default();
    if let Value::Object(map) = values {
        config.push_layer("test", map);
    }
    PolicyFacts {
        applications: ApplicationPolicies::from_config(&config),
        ..PolicyFacts::default()
    }
}

#[test]
fn objects_of_uninstalled_applications_are_hidden() {
    let facts = application_facts(json!({
        "phabricator.uninstalled-applications": {"PhabricatorDiffusionApplication": true}
    }));
    let repository = object_with_phid("PHID-REPO-a", "users");
    assert!(!can_view("admin", &repository, &facts));
    assert!(can_view("omnipotent", &repository, &facts));
}

#[test]
fn objects_of_restricted_applications_are_hidden() {
    let facts = application_facts(json!({
        "phabricator.application-settings": {
            "PHID-APPS-PhabricatorDifferentialApplication": {"policy": {"view": "admin"}}
        }
    }));
    let revision = object_with_phid("PHID-DREV-a", "users");
    assert_eq!(visibility(&revision, &facts), [false, false, true]);
    assert!(
        !can_see_application(
            &viewer("user"),
            "PhabricatorDifferentialApplication",
            &facts
        )
        .unwrap()
    );
    assert!(can_see_application(&viewer("user"), "PhabricatorFeedApplication", &facts).unwrap());
}

fn space(phid: &str, policy: &str, is_default: bool) -> (Phid, Space) {
    (
        Phid::new(phid),
        Space {
            subject: object_with_phid(phid, policy),
            is_default,
        },
    )
}

#[test]
fn spaces() {
    let mut subject = object("users");
    subject.space = Some(None);
    let mut facts = PolicyFacts::default();
    assert!(can_view("user", &subject, &facts));

    subject.space = Some(Some(Phid::new("PHID-SPCE-secret")));
    assert!(!can_view("user", &subject, &facts));

    facts.spaces = HashMap::from([
        space("PHID-SPCE-default", "users", true),
        space("PHID-SPCE-secret", "admin", false),
    ]);
    assert_eq!(visibility(&subject, &facts), [false, false, true]);

    subject.space = Some(None);
    assert_eq!(visibility(&subject, &facts), [false, true, true]);

    subject.space = Some(Some(Phid::new("PHID-SPCE-unknown")));
    assert!(!can_view("admin", &subject, &facts));

    // Automatic capabilities don't bypass spaces.
    subject.space = Some(Some(Phid::new("PHID-SPCE-secret")));
    subject.automatic = vec![AutomaticCapability {
        principal: Phid::new("PHID-USER-user"),
        capabilities: None,
    }];
    assert!(!can_view("user", &subject, &facts));
}

#[test]
fn object_policies() {
    let mut revision = PolicySubject::new(Some(Phid::new("PHID-DREV-1")), ObjectKind::Revision)
        .with_policy(Capability::VIEW, "obj.differential.reviewers");
    revision.subscribable = true;
    let mut facts = PolicyFacts::default();
    assert!(!can_view("user", &revision, &facts));
    facts.reviewing.insert(Phid::new("PHID-DREV-1"));
    assert!(can_view("user", &revision, &facts));
    assert!(!can_view("public", &revision, &facts));

    revision = revision.with_policy(Capability::VIEW, "obj.subscriptions.subscribers");
    assert!(!can_view("user", &revision, &facts));
    revision.implicit_subscribers = vec![Phid::new("PHID-USER-user")];
    assert!(can_view("user", &revision, &facts));
    revision.implicit_subscribers.clear();
    facts.subscribed.insert(Phid::new("PHID-DREV-1"));
    assert!(can_view("user", &revision, &facts));

    // Rules which can't apply to the object reject.
    let other = object_with_phid("PHID-TEST-1", "obj.differential.reviewers");
    facts.reviewing.insert(Phid::new("PHID-TEST-1"));
    assert!(!can_view("user", &other, &facts));
    assert!(!can_view("user", &object("obj.calendar.invitees"), &facts));

    let project = PolicySubject::new(Some(Phid::new("PHID-PROJ-1")), ObjectKind::Project)
        .with_policy(Capability::VIEW, "obj.project.members");
    assert!(!can_view("user", &project, &facts));
    facts
        .viewer_direct_projects
        .insert(Phid::new("PHID-PROJ-1"));
    assert!(can_view("user", &project, &facts));
}

fn custom(rules: Value, default_action: &str) -> PolicyFacts {
    PolicyFacts {
        viewer_projects: [Phid::new("PHID-PROJ-team")].into(),
        custom_policies: HashMap::from([(
            Phid::new("PHID-PLCY-custom"),
            CustomPolicy {
                rules,
                default_action: default_action.to_owned(),
            },
        )]),
        ..PolicyFacts::default()
    }
}

#[rstest]
#[case(
    json!([{"action": "allow", "rule": "PhabricatorUsersPolicyRule", "value": ["PHID-USER-user"]}]),
    "deny",
    [false, true, false]
)]
#[case(
    json!([{"action": "deny", "rule": "PhabricatorUsersPolicyRule", "value": ["PHID-USER-user"]}]),
    "allow",
    [true, false, true]
)]
#[case(
    json!([
        {"action": "allow", "rule": "PhabricatorAdministratorsPolicyRule", "value": null},
        {"action": "allow", "rule": "PhabricatorProjectsPolicyRule", "value": ["PHID-PROJ-team"]},
    ]),
    "deny",
    [false, true, true]
)]
#[case(json!([]), "allow", [true, true, true])]
#[case(json!(["not a rule"]), "allow", [false, false, false])]
#[case(
    json!([{"action": "allow", "rule": "PhabricatorNoSuchPolicyRule", "value": null}]),
    "allow",
    [false, false, false]
)]
#[case(
    json!([{"action": "allow", "rule": "PhabricatorProjectMembersPolicyRule", "value": null}]),
    "allow",
    [false, false, false]
)]
#[case(
    json!([{"rule": "PhabricatorAdministratorsPolicyRule", "value": null}]),
    "deny",
    [false, false, false]
)]
fn custom_policies(
    #[case] rules: Value,
    #[case] default_action: &str,
    #[case] expected: [bool; 3],
) {
    let facts = custom(rules, default_action);
    // Admins in these cases are not members of PHID-PROJ-team, users are.
    let subject = object("PHID-PLCY-custom");
    let result = ["public", "user", "admin"].map(|spec| {
        let mut viewer_facts = facts.clone();
        if spec != "user" {
            viewer_facts.viewer_projects.clear();
        }
        has_capability(&viewer(spec), &subject, Capability::VIEW, &viewer_facts).unwrap()
    });
    assert_eq!(result, expected);
}

#[test]
fn unknown_custom_policy_rejects() {
    assert!(!can_view(
        "admin",
        &object("PHID-PLCY-missing"),
        &PolicyFacts::default()
    ));
}
