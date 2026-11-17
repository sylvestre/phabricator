//! `differential.changeset.search` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-dddddddddddddddddddddddddddd";

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

async fn query(context: &Context, params: Value) -> Response {
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
    call::dispatch(
        context,
        &registry,
        "differential.changeset.search",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn answer(context: &Context, params: Value) -> Value {
    match query(context, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{params}: expected an answer, got {other:?}"),
    }
}

async fn ids(context: &Context, params: Value) -> Vec<i64> {
    answer(context, params).await["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|changeset| changeset["id"].as_i64().expect("id"))
        .collect()
}

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
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-open', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-1'), \
             (2, 'Hidden', 'PHID-DREV-hidden', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'admin', 'admin', NULL, '{{}}', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (2, 'PHID-DIFF-2', 2, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (3, 'PHID-DIFF-3', NULL, 'PHID-USER-bob', 0, 0, 1, 1, 1, 'users'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, diffID, filename, \
               changeType, fileType, addLines, delLines, dateCreated, dateModified, phid) VALUES \
             (1, 1, 'src/main.rs', 2, 1, 3, 0, 10, 11, 'PHID-DCNG-1'), \
             (2, 1, 'src', 2, 4, 0, 0, 10, 11, 'PHID-DCNG-2'), \
             (3, 2, 'secret.rs', 2, 1, 1, 0, 10, 11, 'PHID-DCNG-3'), \
             (4, 3, 'alone.rs', 2, 1, 1, 0, 10, 11, 'PHID-DCNG-4')"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn describes_changesets() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let result = answer(
        &context,
        json!({"constraints": {"diffPHIDs": ["PHID-DIFF-1"]}}),
    )
    .await;
    assert_eq!(
        result["data"],
        json!([
            {
                "id": 2,
                "type": "DCNG",
                "phid": "PHID-DCNG-2",
                "fields": {
                    "diffPHID": "PHID-DIFF-1",
                    "path": {"displayPath": "src/", "absolutePath.base64": "L3NyYw=="},
                    "dateCreated": 10,
                    "dateModified": 11,
                    "policy": {"view": "users"},
                },
                "attachments": {},
            },
            {
                "id": 1,
                "type": "DCNG",
                "phid": "PHID-DCNG-1",
                "fields": {
                    "diffPHID": "PHID-DIFF-1",
                    "path": {
                        "displayPath": "src/main.rs",
                        "absolutePath.base64": "L3NyYy9tYWluLnJz",
                    },
                    "dateCreated": 10,
                    "dateModified": 11,
                    "policy": {"view": "users"},
                },
                "attachments": {},
            },
        ])
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn follows_the_diffs() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // The hidden revision's diff is hidden too.
    assert_eq!(
        ids(
            &context,
            json!({"constraints": {"ids": [1, 2, 3]}, "order": "oldest"})
        )
        .await,
        [1, 2]
    );
    assert_eq!(
        ids(
            &context,
            json!({"constraints": {"diffPHIDs": ["PHID-DIFF-nope"]}})
        )
        .await,
        [0_i64; 0]
    );
    let first = answer(
        &context,
        json!({"constraints": {"ids": [1, 2]}, "limit": 1}),
    )
    .await;
    assert_eq!(first["cursor"]["after"], "2");
    // PHP fails on changesets of diffs without a revision.
    assert!(matches!(
        query(&context, json!({"constraints": {"ids": [4]}})).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}
