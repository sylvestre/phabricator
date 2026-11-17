//! The reviewer, responsible, subscriber and project constraints of
//! `differential.revision.search` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-rrrrrrrrrrrrrrrrrrrrrrrrrrrr";

const DATABASES: &[&str] = &[
    "differential",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
];

async fn query(context: &Context, constraints: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_differential::register(&mut registry);
    let params = json!({"constraints": constraints, "__conduit__": {"token": TOKEN}});
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    call::dispatch(
        context,
        &registry,
        "differential.revision.search",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

/// The IDs of the revisions found, in order.
async fn ids(context: &Context, constraints: Value) -> Vec<i64> {
    match query(context, constraints.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result["data"]
            .as_array()
            .expect("data")
            .iter()
            .map(|revision| revision["id"].as_i64().expect("id"))
            .collect(),
        other => panic!("{constraints}: expected an answer, got {other:?}"),
    }
}

/// Revisions `(id, author)`.
const REVISIONS: &[(i64, &str)] = &[
    (1, "alice"),
    (2, "bob"),
    (3, "carol"),
    (4, "dave"),
    (5, "alice"),
];

/// Reviewers `(revision, reviewer, status)`.
const REVIEWERS: &[(i64, &str, &str)] = &[
    (1, "PHID-USER-bob", "added"),
    (1, "PHID-PROJ-front", "added"),
    (2, "PHID-USER-carol", "resigned"),
    (3, "PHID-USER-alice", "accepted"),
];

/// Edges `(revision, type, destination)`: subscribers (21) and projects
/// (41).
const EDGES: &[(i64, i64, &str)] = &[
    (4, 21, "PHID-USER-carol"),
    (5, 21, "PHID-USER-bob"),
    (4, 41, "PHID-PROJ-secure"),
    (5, 41, "PHID-PROJ-secure"),
    (5, 41, "PHID-PROJ-other"),
    (5, 41, "PHID-PROJ-parent"),
];

/// Projects `(id, PHID, parent)`.
const PROJECTS: &[(i64, &str, &str)] = &[
    (1, "PHID-PROJ-front", "NULL"),
    (2, "PHID-PROJ-secure", "NULL"),
    (3, "PHID-PROJ-other", "NULL"),
    (4, "PHID-PROJ-parent", "NULL"),
    (5, "PHID-PROJ-child", "'PHID-PROJ-parent'"),
];

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-PROJ-front', 60, 'PHID-USER-dave', 1, 0)"
        ))
        .await;
    for (id, author) in REVISIONS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, \
                   status, summary, testPlan, authorPHID, dateCreated, dateModified, attached, \
                   mailKey, viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) \
                 VALUES ({id}, 'R{id}', 'PHID-DREV-{id}', 'needs-review', '', '', \
                   'PHID-USER-{author}', {id}, {id}, '{{}}', 'k', 'users', 'users', NULL, '{{}}', \
                   'PHID-DIFF-{id}'); \
                 INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
                   authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
                   viewPolicy) VALUES \
                 ({id}, 'PHID-DIFF-{id}', {id}, 'PHID-USER-{author}', 0, 0, 1, 1, 1, 'users')"
            ))
            .await;
    }
    for (revision, reviewer, status) in REVIEWERS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, \
                   reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
                 ('PHID-DREV-{revision}', '{reviewer}', '{status}', 1, 1, '{{}}')"
            ))
            .await;
    }
    for (revision, edge_type, destination) in EDGES {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
                 ('PHID-DREV-{revision}', {edge_type}, '{destination}', 1, 0)"
            ))
            .await;
    }
    for (id, phid, parent) in PROJECTS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, \
                   dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
                   hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
                   projectPathKey, properties, subtype, parentProjectPHID, milestoneNumber, \
                   primarySlug) VALUES \
                 ({id}, 'P{id}', '{phid}', 'PHID-USER-x', 1, 1, '0', 'users', 'users', 'users', \
                   'fa-x', 'blue', 'k', 0, 0, 0, 'p{id}', 0, 'k{id:03}', '{{}}', 'default', \
                   {parent}, NULL, NULL)"
            ))
            .await;
    }
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn finds_reviewed_revisions() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(
        ids(&context, json!({"reviewerPHIDs": ["PHID-USER-bob"]})).await,
        [1]
    );
    // Reviewers who resigned don't count.
    assert_eq!(
        ids(&context, json!({"reviewerPHIDs": ["PHID-USER-carol"]})).await,
        [0_i64; 0]
    );
    assert_eq!(
        ids(
            &context,
            json!({"reviewerPHIDs": ["PHID-PROJ-front", "PHID-USER-alice"]})
        )
        .await,
        [3, 1]
    );
    assert_eq!(
        ids(
            &context,
            json!({"reviewerPHIDs": ["PHID-USER-alice"], "authorPHIDs": ["PHID-USER-carol"]})
        )
        .await,
        [3]
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn finds_what_users_are_responsible_for() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // Authored, or reviewed directly or by a project they are a member of.
    assert_eq!(
        ids(&context, json!({"responsiblePHIDs": ["PHID-USER-dave"]})).await,
        [4, 1]
    );
    assert_eq!(
        ids(&context, json!({"responsiblePHIDs": ["PHID-USER-alice"]})).await,
        [5, 3, 1]
    );
    // A union of the revisions they or the authors wrote, and those they
    // review which the authors wrote.
    assert_eq!(
        ids(
            &context,
            json!({"responsiblePHIDs": ["PHID-USER-alice"], "authorPHIDs": ["PHID-USER-bob"]})
        )
        .await,
        [5, 2, 1]
    );
    assert_eq!(
        ids(
            &context,
            json!({"responsiblePHIDs": ["PHID-USER-dave"], "reviewerPHIDs": ["PHID-USER-bob"]})
        )
        .await,
        [1]
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn finds_subscribed_and_tagged_revisions() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // Any of the subscribers.
    assert_eq!(
        ids(
            &context,
            json!({"subscribers": ["PHID-USER-carol", "PHID-USER-bob"]})
        )
        .await,
        [5, 4]
    );
    // Every project.
    assert_eq!(
        ids(&context, json!({"projects": ["PHID-PROJ-secure"]})).await,
        [5, 4]
    );
    assert_eq!(
        ids(
            &context,
            json!({"projects": ["PHID-PROJ-secure", "PHID-PROJ-other"]})
        )
        .await,
        [5]
    );
    assert_eq!(
        ids(&context, json!({"projects": ["PHID-PROJ-front"]})).await,
        [0_i64; 0]
    );
    // Empty user and project lists are no constraint.
    assert_eq!(
        ids(&context, json!({"subscribers": [], "projects": []})).await,
        [5, 4, 3, 2, 1]
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_the_rest_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let Response::Envelope(envelope) = query(&context, json!({"reviewerPHIDs": []})).await else {
        panic!("expected an error");
    };
    assert_eq!(envelope.error_code.as_deref(), Some("ERR-CONDUIT-CORE"));
    for constraints in [
        // Its subproject would match too.
        json!({"projects": ["PHID-PROJ-parent"]}),
        json!({"projects": ["other"]}),
        json!({"reviewerPHIDs": ["none()"]}),
        json!({"responsiblePHIDs": ["viewer()"]}),
        json!({"subscribers": ["carol"]}),
    ] {
        assert!(
            matches!(
                query(&context, constraints.clone()).await,
                Response::Fallback(_)
            ),
            "{constraints}"
        );
    }

    test_db.drop_databases().await;
}
