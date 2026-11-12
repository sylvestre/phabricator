use std::collections::HashMap;

use phab_core::{Config, Phid};
use phab_db::testing::TestDatabase;
use phab_policy::loader::{LoaderError, NoExtendedObjects, SubjectResolver};
use phab_policy::{Capability, ExtendedPolicy, ObjectKind, PolicySubject, Viewer, loader};

const DATABASES: &[&str] = &["project", "user", "differential", "policy", "spaces"];

struct Repositories;

impl SubjectResolver for Repositories {
    async fn resolve(&self, phids: &[Phid]) -> Result<HashMap<Phid, PolicySubject>, LoaderError> {
        Ok(phids
            .iter()
            .filter(|phid| phid.as_str() == "PHID-REPO-private")
            .map(|phid| {
                let subject = PolicySubject::new(Some(phid.clone()), ObjectKind::Other)
                    .with_policy(Capability::VIEW, "PHID-PLCY-team");
                (phid.clone(), subject)
            })
            .collect())
    }
}

fn revision(phid: &str, policy: &str) -> PolicySubject {
    let mut subject = PolicySubject::new(Some(Phid::new(phid)), ObjectKind::Revision)
        .with_policy(Capability::VIEW, policy);
    subject.subscribable = true;
    subject
}

#[tokio::test]
async fn loads_memberships_reviewers_subscriptions_and_custom_policies() {
    let Some(test_db) = TestDatabase::create(DATABASES).await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_project`.project (name, phid, authorPHID, dateCreated, dateModified, \
               status, viewPolicy, editPolicy, joinPolicy, isMembershipLocked, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, projectPathKey, \
               properties, subtype) \
             VALUES ('Team', 'PHID-PROJ-team', 'PHID-USER-x', 1, 1, '0', 'users', 'users', 'users', \
               0, 'project', 'blue', 'mk', 0, 0, 0, 'aaaa', 0, 'aaaa', '{}', 'default'); \
             INSERT INTO `{ns}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-PROJ-team', 60, 'PHID-USER-alice', 1, 0); \
             INSERT INTO `{ns}_user`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-USER-bob', 22, 'PHID-DREV-subscribed', 1, 0); \
             INSERT INTO `{ns}_differential`.differential_reviewer \
               (revisionPHID, reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
               ('PHID-DREV-reviewed', 'PHID-PROJ-team', 'added', 1, 1, '{}'); \
             INSERT INTO `{ns}_policy`.policy (phid, rules, defaultAction, dateCreated, dateModified) \
             VALUES ('PHID-PLCY-team', \
               '[{\"action\":\"allow\",\"rule\":\"PhabricatorProjectsPolicyRule\",\"value\":[\"PHID-PROJ-team\"]}]', \
               'deny', 1, 1)",
        )
        .await;

    let database = test_db.database();
    let config = Config::default();
    let reviewed = revision("PHID-DREV-reviewed", "obj.differential.reviewers");
    let subscribed = revision("PHID-DREV-subscribed", "obj.subscriptions.subscribers");
    let custom = revision("PHID-DREV-custom", "PHID-PLCY-team");
    let mut in_private_repository = revision("PHID-DREV-repo", "users");
    in_private_repository.extended = vec![ExtendedPolicy {
        for_capability: Capability::VIEW,
        object: Phid::new("PHID-REPO-private"),
        capabilities: vec![Capability::VIEW],
    }];
    let subjects = [&reviewed, &subscribed, &custom, &in_private_repository];

    let alice = Viewer::user(Phid::new("PHID-USER-alice"), false);
    let facts = loader::load_facts(database, &config, &alice, &subjects, &Repositories, 0)
        .await
        .unwrap();
    assert!(facts.viewer_projects.contains(&Phid::new("PHID-PROJ-team")));
    let visible = phab_policy::filter(&alice, &subjects, &[Capability::VIEW], &facts).unwrap();
    assert_eq!(visible, [true, false, true, true]);

    let bob = Viewer::user(Phid::new("PHID-USER-bob"), false);
    let facts = loader::load_facts(database, &config, &bob, &subjects, &Repositories, 0)
        .await
        .unwrap();
    let visible = phab_policy::filter(&bob, &subjects, &[Capability::VIEW], &facts).unwrap();
    assert_eq!(visible, [false, true, false, false]);

    let anonymous = Viewer::anonymous();
    let facts = loader::load_facts(
        database,
        &config,
        &anonymous,
        &subjects,
        &NoExtendedObjects,
        0,
    )
    .await
    .unwrap();
    let visible = phab_policy::filter(&anonymous, &subjects, &[Capability::VIEW], &facts).unwrap();
    assert_eq!(visible, [false, false, false, false]);

    test_db.drop_databases().await;
}
