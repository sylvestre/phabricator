//! `differential.querydiffs`, `differential.getrawdiff` and
//! `differential.getcommitpaths` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-qqqqqqqqqqqqqqqqqqqqqqqqqqqq";

const DATABASES: &[&str] = &[
    "differential",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "file",
];

async fn call(context: &Context, method: &str, params: Value) -> Response {
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
        method,
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn answer(context: &Context, params: Value) -> Value {
    match call(context, "differential.querydiffs", params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{params}: expected an answer, got {other:?}"),
    }
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
               viewPolicy, branch, creationMethod) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', 0, 4, 1, 5, 6, 'users', 'main', 'arc'), \
             (2, 'PHID-DIFF-2', 2, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users', NULL, 'arc'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, newProperties, \
               addLines, delLines, dateCreated, dateModified, phid) VALUES \
             (10, 1, 'a.txt', 'a.txt', '[]', 2, 1, '{{\"line:first\":1}}', '{{}}', \
               '{{\"unix:filemode\":\"100755\"}}', 1, 1, 1, 1, 'PHID-DCNG-10'); \
             INSERT INTO `{{ns}}_differential`.differential_hunk (id, changesetID, oldOffset, \
               oldLen, newOffset, newLen, dataType, dataEncoding, dataFormat, data, dateCreated, \
               dateModified) VALUES \
             (20, 10, 1, 1, 1, 1, 'text', 'utf8', 'byte', '-x\\n+y\\n', 1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_diffproperty (diffID, name, data, \
               dateCreated, dateModified) VALUES \
             (1, 'local:commits', \
               '{{\"abc\":{{\"author\":\"Alice\",\"authorEmail\":\"alice@example.com\"}}}}', 1, 1), \
             (1, 'arc:unit', '{{}}', 1, 1)"
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
async fn describes_diffs_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // Integers stay MySQL's strings; empty objects are PHP's empty arrays.
    let expected = json!({
        "1": {
            "id": "1",
            "revisionID": "1",
            "dateCreated": "5",
            "dateModified": "6",
            "sourceControlBaseRevision": null,
            "sourceControlPath": null,
            "sourceControlSystem": null,
            "branch": "main",
            "bookmark": null,
            "creationMethod": "arc",
            "description": null,
            "unitStatus": "4",
            "lintStatus": "0",
            "changes": [{
                "id": "10",
                "metadata": {"line:first": 1},
                "oldPath": "a.txt",
                "currentPath": "a.txt",
                "awayPaths": [],
                "oldProperties": [],
                "newProperties": {"unix:filemode": "100755"},
                "type": "2",
                "fileType": "1",
                "commitHash": null,
                "addLines": "1",
                "delLines": "1",
                "hunks": [{
                    "oldOffset": "1",
                    "newOffset": "1",
                    "oldLength": "1",
                    "newLength": "1",
                    "addLines": null,
                    "delLines": null,
                    "isMissingOldNewline": null,
                    "isMissingNewNewline": null,
                    "corpus": "-x\n+y\n",
                }],
            }],
            "properties": {
                "local:commits": {"abc": {"author": "Alice", "authorEmail": "alice@example.com"}},
                "arc:unit": [],
            },
            "authorName": "Alice",
            "authorEmail": "alice@example.com",
        },
    });
    assert_eq!(answer(&context, json!({"ids": [1]})).await, expected);
    // The hidden revision's diff is left out.
    assert_eq!(
        answer(&context, json!({"revisionIDs": [1, 2]})).await,
        expected
    );
    assert_eq!(answer(&context, json!({"ids": [2]})).await, json!([]));
    assert_eq!(answer(&context, json!({})).await, json!([]));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn renders_git_patches() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let Response::Envelope(envelope) =
        call(&context, "differential.getrawdiff", json!({"diffID": 1})).await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        envelope.result,
        json!(
            "diff --git a/a.txt b/a.txt\nold mode 100644\nnew mode 100755\n--- a/a.txt\n\
             +++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\n\n"
        )
    );
    // The hidden revision's diff is not found.
    let Response::Envelope(envelope) =
        call(&context, "differential.getrawdiff", json!({"diffID": 2})).await
    else {
        panic!("expected an error");
    };
    assert_eq!(envelope.error_code.as_deref(), Some("ERR_NOT_FOUND"));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn lists_commit_paths() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // A newer diff, without a revision visible to Bob, wins.
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy, branch, creationMethod) VALUES \
             (3, 'PHID-DIFF-3', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'no-one', NULL, 'arc'); \
             INSERT INTO `{ns}_differential`.differential_changeset (id, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, newProperties, \
               addLines, delLines, dateCreated, dateModified, phid) VALUES \
             (12, 3, NULL, 'z.txt', '[]', 1, 1, '{}', '{}', '{}', 1, 0, 1, 1, 'PHID-DCNG-12'), \
             (11, 3, 'a.txt', 'b.txt', '[]', 6, 1, '{}', '{}', '{}', 0, 0, 1, 1, 'PHID-DCNG-11'); \
             INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (4, 'Empty', 'PHID-DREV-empty', 'draft', '', '', 'PHID-USER-alice', 1, 1, '{}', \
               'k', 'users', 'users', NULL, '{}', '')",
        )
        .await;

    let paths = |params: Value| {
        let context = &context;
        async move { call(context, "differential.getcommitpaths", params).await }
    };
    let Response::Envelope(envelope) = paths(json!({"revision_id": "1"})).await else {
        panic!("expected an answer");
    };
    assert_eq!(envelope.result, json!(["b.txt", "z.txt"]));
    let Response::Envelope(envelope) = paths(json!({"revision_id": 99})).await else {
        panic!("expected an error");
    };
    assert_eq!(envelope.error_code.as_deref(), Some("ERR_NOT_FOUND"));
    // PHP raises a policy exception for the hidden revision, and fails on
    // a revision without a diff.
    for id in [2, 4] {
        assert!(matches!(
            paths(json!({"revision_id": id})).await,
            Response::Fallback(_)
        ));
    }

    test_db.drop_databases().await;
}
