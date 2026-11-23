//! The frozen Differential write methods (`differential.createcomment`,
//! `differential.createrevision`, `differential.updaterevision`,
//! `differential.close`) against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

/// Bob's token.
const BOB: &str = "api-bbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// Alice's token.
const ALICE: &str = "api-aaaaaaaaaaaaaaaaaaaaaaaaaaaa";

const DATABASES: &[&str] = &[
    "differential",
    "audit",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "herald",
    "worker",
    "harbormaster",
    "file",
];

async fn invoke(context: &Context, method: &str, token: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_differential::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": token});
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
        Mode::Serve,
        1_700_000_000,
    )
    .await
    .response
}

/// Bob and Alice; D1, Alice's, which Bob rejected and Carol accepted, on
/// diff 1; D2 on diff 2; the unattached diff 101 of Alice, in a Git
/// repository, with two files, its local commit and a view state.
async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{BOB}', NULL, 1, 1), \
             ('PHID-USER-alice', 'api', '{ALICE}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-git', 'Git', NULL, 'git', '{{}}', 1, 1, 'users', 'admin', 'admin'); \
             INSERT INTO `{{ns}}_repository`.repository_path (id, path, pathHash) VALUES \
             (7, '/', MD5('/')); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-revision', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-1'), \
             (2, 'Other', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_reviewer (id, revisionPHID, \
               reviewerPHID, reviewerStatus, dateCreated, dateModified, options, \
               lastActionDiffPHID) VALUES \
             (5, 'PHID-DREV-1', 'PHID-USER-bob', 'rejected', 1, 1, '{{}}', 'PHID-DIFF-1'), \
             (6, 'PHID-DREV-1', 'PHID-USER-carol', 'accepted', 1, 1, '{{}}', 'PHID-DIFF-1'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, repositoryPHID, sourceControlSystem, sourceControlPath, lintStatus, \
               unitStatus, lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', NULL, 'git', '/', 0, 0, 1, 1, 1, \
               'PHID-USER-alice'), \
             (2, 'PHID-DIFF-2', 2, 'PHID-USER-alice', NULL, 'git', '/', 0, 0, 1, 1, 1, \
               'PHID-USER-alice'), \
             (101, 'PHID-DIFF-new', NULL, 'PHID-USER-alice', 'PHID-REPO-git', 'git', '/', 0, \
               0, 4, 1, 1, 'PHID-USER-alice'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, \
               newProperties, addLines, delLines, dateCreated, dateModified, phid) VALUES \
             (11, 101, 'README.md', 'README.md', '[]', 2, 1, '{{}}', '{{}}', '{{}}', 2, 1, 1, 1, \
               'PHID-DCNG-11'), \
             (12, 101, NULL, 'docs/a.md', '[]', 1, 1, '{{}}', '{{}}', '{{}}', 1, 0, 1, 1, \
               'PHID-DCNG-12'); \
             INSERT INTO `{{ns}}_differential`.differential_diffproperty (diffID, name, data, \
               dateCreated, dateModified) VALUES \
             (101, 'local:commits', '{{\"aaa\":{{\"commit\":\"aaa\",\"tree\":\"ttt\"}}}}', 1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_viewstate (viewerPHID, objectPHID, \
               viewState, dateCreated, dateModified) VALUES \
             ('PHID-USER-alice', 'PHID-DIFF-new', '{{\"x\":1}}', 1, 1)"
        ))
        .await;
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "http://phab.test/"}) {
        config.push_layer("test", map);
    }
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn values(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| phab_db::rows::text(row, "value").expect("a value"))
        .collect()
}

fn answer(response: Response) -> Value {
    let Response::Envelope(Envelope {
        result,
        error_code: None,
        ..
    }) = response
    else {
        panic!("expected an answer");
    };
    result
}

/// The `error_code` and `error_info` of an error.
fn error(response: Response) -> (String, Value) {
    let Response::Envelope(Envelope {
        error_code: Some(code),
        error_info,
        ..
    }) = response
    else {
        panic!("expected an error");
    };
    (code, json!(error_info))
}

/// The transactions' types and content sources, in order.
async fn transaction_types(test_db: &TestDatabase) -> Vec<String> {
    values(
        test_db,
        "SELECT CONCAT_WS(' ', transactionType, contentSource) AS value FROM \
         `{ns}_differential`.differential_transaction ORDER BY id",
    )
    .await
}

/// The transactions' types, in order.
async fn types_only(test_db: &TestDatabase) -> Vec<String> {
    transaction_types(test_db)
        .await
        .iter()
        .map(|row| row.split(' ').next().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn comments_and_acts_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let params = json!({"revision_id": 1, "action": "accept", "message": "Fine now.",
        "silent": true});
    let result = answer(invoke(&context, "differential.createcomment", BOB, params).await);
    assert_eq!(
        result,
        json!({"revisionid": "1", "uri": "http://phab.test/D1"})
    );
    let conduit = r#"{"source":"conduit","params":[]}"#;
    let rows = transaction_types(&test_db).await;
    assert_eq!(rows[0], format!("differential.revision.accept {conduit}"));
    assert!(
        rows.contains(&format!("core:comment {conduit}")),
        "{rows:?}"
    );
    assert_eq!(
        values(
            &test_db,
            "SELECT CONCAT(content, ' ', contentSource) AS value FROM \
             `{ns}_differential`.differential_transaction_comment"
        )
        .await,
        [format!("Fine now. {conduit}")]
    );
    assert_eq!(
        values(
            &test_db,
            "SELECT CONCAT(reviewerPHID, ' ', reviewerStatus) AS value FROM \
             `{ns}_differential`.differential_reviewer ORDER BY id"
        )
        .await,
        ["PHID-USER-bob accepted", "PHID-USER-carol accepted"]
    );
}

#[tokio::test]
async fn refuses_comments_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let comment = |params| invoke(&context, "differential.createcomment", BOB, params);
    assert_eq!(
        error(comment(json!({"revision_id": 99, "action": "bogus"})).await),
        ("ERR_BAD_REVISION".to_owned(), json!("Bad revision ID."))
    );
    assert_eq!(
        error(comment(json!({"revision_id": "1", "action": "abandon"})).await),
        (
            "ERR-CONDUIT-CORE".to_owned(),
            json!("Unsupported action \"abandon\".")
        )
    );
    // Bob's draft inline comment on D1.
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_transaction_comment (phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, revisionPHID, changesetID, \
               isNewFile, lineNumber, lineLength, hasReplies, attributes) VALUES \
             ('PHID-XCMT-draft', NULL, 'PHID-USER-bob', 'public', 'PHID-USER-bob', 1, 'Nit', \
               '{}', 0, 1, 1, 'PHID-DREV-1', 11, 1, 1, 0, 0, '{}')",
        )
        .await;
    let refused = [
        json!({"revision_id": 1, "message": "Hi", "attach_inlines": true}),
        // "comment" without a message applies nothing, left to PHP.
        json!({"revision_id": 1, "action": "comment"}),
        json!({"revision_id": 1, "action": 1, "message": "Hi"}),
        json!({"revision_id": "D1", "message": "Hi"}),
    ];
    for params in refused {
        assert!(
            matches!(comment(params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }
    assert_eq!(transaction_types(&test_db).await, Vec::<String>::new());
    // Drafts of others don't count.
    answer(
        invoke(
            &context,
            "differential.createcomment",
            ALICE,
            json!({"revision_id": 1, "message": "Thanks.", "attach_inlines": true}),
        )
        .await,
    );
    assert!(
        types_only(&test_db)
            .await
            .contains(&"core:comment".to_owned())
    );
}

#[tokio::test]
async fn creates_revisions_from_fields_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let params = json!({
        "user": "ignored",
        "diffid": 101,
        "fields": {
            "title": "Add the docs",
            "summary": "Documents it.",
            "revisionID": null,
            "reviewerPHIDs": ["PHID-USER-bob!"],
            "reviewedByPHIDs": [],
            "unknown": 5,
            "testPlan": "Read it.",
        },
    });
    let result = answer(invoke(&context, "differential.createrevision", ALICE, params).await);
    assert_eq!(
        result,
        json!({"revisionid": 3, "uri": "http://phab.test/D3"})
    );
    assert_eq!(
        types_only(&test_db).await,
        [
            "core:create",
            "differential:update",
            "differential.revision.title",
            "differential.revision.summary",
            "differential.revision.reviewers",
            "differential.revision.testplan",
        ]
    );
    assert_eq!(
        values(
            &test_db,
            "SELECT CONCAT(reviewerPHID, ' ', reviewerStatus) AS value FROM \
             `{ns}_differential`.differential_reviewer WHERE revisionPHID NOT IN \
             ('PHID-DREV-1', 'PHID-DREV-2')"
        )
        .await,
        ["PHID-USER-bob blocking"]
    );
}

#[tokio::test]
async fn updates_revisions_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let params = json!({
        "id": 1,
        "diffid": "101",
        "fields": {"revisionID": 1},
        "message": "Address the comments\r\n\nAnd more.",
    });
    let result = answer(invoke(&context, "differential.updaterevision", ALICE, params).await);
    assert_eq!(
        result,
        json!({"revisionid": "1", "uri": "http://phab.test/D1"})
    );
    let types = types_only(&test_db).await;
    assert_eq!(types[0], "differential:update");
    assert!(types.contains(&"core:comment".to_owned()), "{types:?}");
    assert_eq!(
        values(
            &test_db,
            "SELECT CONCAT_WS(' ', description, revisionID, dateModified) AS value FROM \
             `{ns}_differential`.differential_diff WHERE id = 101"
        )
        .await,
        ["Address the comments 1 1700000000"]
    );
}

#[tokio::test]
async fn refuses_updates_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_revision SET status = 'published' WHERE \
             id = 2",
        )
        .await;
    let update = |params| invoke(&context, "differential.updaterevision", ALICE, params);
    let message = "Update";
    assert_eq!(
        error(update(json!({"id": 1, "diffid": 999, "fields": {}, "message": message})).await).0,
        "ERR_BAD_DIFF"
    );
    assert_eq!(
        error(update(json!({"id": 99, "diffid": 101, "fields": {}, "message": message})).await).0,
        "ERR_BAD_REVISION"
    );
    assert_eq!(
        error(update(json!({"id": 2, "diffid": 101, "fields": {}, "message": message})).await),
        (
            "ERR_CLOSED".to_owned(),
            json!("This revision has already been closed.")
        )
    );
    assert_eq!(
        error(
            update(json!({"id": 1, "diffid": 101, "fields": {"title": 5}, "message": message}))
                .await
        ),
        (
            "ERR-CONDUIT-CORE".to_owned(),
            json!(
                "Field \"title\" expects a string value, but received a value of type \
                 \"integer\"."
            )
        )
    );
    // Reviewers with a new diff, left to PHP: nothing is written.
    let params = json!({"id": 1, "diffid": 101, "fields": {"reviewerPHIDs": []},
        "message": message});
    assert!(matches!(update(params).await, Response::Fallback(_)));
    assert_eq!(
        values(
            &test_db,
            "SELECT CONCAT('[', COALESCE(description, ''), ']') AS value FROM \
             `{ns}_differential`.differential_diff WHERE id = 101"
        )
        .await,
        ["[]"]
    );
    assert_eq!(transaction_types(&test_db).await, Vec::<String>::new());
}

#[tokio::test]
async fn closes_through_the_editor() {
    let Some((_test_db, context)) = setup().await else {
        return;
    };
    let close = |params| invoke(&context, "differential.close", ALICE, params);
    assert_eq!(
        error(close(json!({"revisionID": 99})).await),
        ("ERR_NOT_FOUND".to_owned(), json!("Revision was not found."))
    );
    // D1 isn't accepted: PHP refuses to close it.
    assert!(matches!(
        close(json!({"revisionID": 1})).await,
        Response::Fallback(_)
    ));
}
