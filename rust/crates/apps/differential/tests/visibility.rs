//! `differential.custom.revision.visibility` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-vsvsvsvsvsvsvsvsvsvsvsvsvsvs";

async fn visibility(context: &Context, params: Value) -> Envelope {
    let mut registry = Registry::default();
    phab_app_differential::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": TOKEN});
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    match call::dispatch(
        context,
        &registry,
        "differential.custom.revision.visibility",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
    {
        Response::Envelope(envelope) => envelope,
        other @ Response::Fallback(_) => panic!("{params}: expected an envelope, got {other:?}"),
    }
}

/// Revisions `(id, view policy)`, none of which Bob can see but D2.
const REVISIONS: &[(i64, &str)] = &[
    (1, "PHID-PLCY-default"),
    (2, "users"),
    (3, "PHID-PLCY-processed"),
    (4, "admin"),
];

#[tokio::test]
async fn classifies_revisions_bob_cannot_see() {
    let Some(test_db) = TestDatabase::create(&[
        "differential",
        "project",
        "user",
        "policy",
        "spaces",
        "conduit",
    ])
    .await
    else {
        return;
    };
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug) VALUES \
             (1, 'secure-revision', 'PHID-PROJ-secure', 'PHID-USER-x', 1, 1, '0', 'admin', \
               'admin', 'admin', 'project', 'red', 'k', 0, 0, 0, 'p1', 0, 'k001', '{{}}', \
               'default', 'secure-revision'); \
             INSERT INTO `{{ns}}_policy`.policy (phid, rules, defaultAction, dateCreated, \
               dateModified) VALUES \
             ('PHID-PLCY-default', '[{{\"action\":\"allow\",\"rule\":\"PhabricatorProjectsPolicyRule\",\"value\":[\"PHID-PROJ-secure\"]}}]', 'deny', 1, 1), \
             ('PHID-PLCY-processed', '[{{\"action\":\"deny\",\"rule\":\"PhabricatorProjectsPolicyRule\",\"value\":[\"PHID-PROJ-secure\"]}},{{\"action\":\"allow\",\"rule\":\"PhabricatorProjectsAllPolicyRule\",\"value\":[\"PHID-PROJ-bmo\"]}}]', 'deny', 1, 1)"
        ))
        .await;
    for (id, view) in REVISIONS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, \
                   status, summary, testPlan, authorPHID, dateCreated, dateModified, attached, \
                   mailKey, viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) \
                   VALUES ({id}, 'R{id}', 'PHID-DREV-{id}', 'needs-review', '', '', \
                   'PHID-USER-alice', 1, 1, '{{}}', 'k', '{view}', 'admin', NULL, '{{}}', '')"
            ))
            .await;
    }
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };

    for (id, expected) in [
        (json!(1), "unprocessed"),
        (json!("2"), "public"),
        (json!(3), "secure"),
        (json!(4), "secure"),
    ] {
        let envelope = visibility(&context, json!({"revisionID": id})).await;
        assert_eq!(
            envelope.result["visibility"], expected,
            "{id}: {envelope:?}"
        );
    }
    assert_eq!(
        visibility(&context, json!({"revisionID": 3})).await.result,
        json!({"revisionID": "3", "visibility": "secure"})
    );
    let envelope = visibility(&context, json!({"revisionID": 99})).await;
    assert_eq!(
        (
            envelope.error_code.as_deref(),
            envelope.error_info.as_deref()
        ),
        (
            Some("ERR-REVISION-NOT-FOUND"),
            Some("Revision D99 not found.")
        )
    );
    let envelope = visibility(&context, json!({})).await;
    assert_eq!(
        envelope.error_code.as_deref(),
        Some("ERR-INVALID-PARAMETER")
    );

    test_db.drop_databases().await;
}
