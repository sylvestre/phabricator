//! `differential.revision.edit` creating revisions and updating their diff
//! against a test database.

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

async fn edit(context: &Context, token: &str, params: Value) -> Response {
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
        "differential.revision.edit",
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
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
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

fn creation(extra: &[Value]) -> Value {
    let mut transactions = vec![
        json!({"type": "update", "value": "PHID-DIFF-new"}),
        json!({"type": "title", "value": "Add the docs"}),
        json!({"type": "summary", "value": "Documents it.\n\nSee the guide."}),
        json!({"type": "testPlan", "value": "Read it."}),
    ];
    transactions.extend(extra.iter().cloned());
    json!({"transactions": transactions})
}

#[tokio::test]
async fn creates_a_revision_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let params = creation(&[json!({"type": "reviewers.add", "value": ["PHID-USER-bob"]})]);
    let result = answer(edit(&context, ALICE, params).await);
    assert_eq!(result["object"]["id"], json!(3));
    let phid = result["object"]["phid"].as_str().unwrap_or_default();
    assert!(phid.starts_with("PHID-DREV-"), "{phid}");
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(6));

    let value = |sql: &'static str| values(&test_db, sql);
    assert_eq!(
        value(
            "SELECT CONCAT_WS(' ', title, status, authorPHID, lineCount, viewPolicy, editPolicy, \
             repositoryPHID, activeDiffPHID, attached, LENGTH(mailKey), properties, summary) AS \
             value FROM `{ns}_differential`.differential_revision WHERE id = 3"
        )
        .await,
        [
            "Add the docs draft PHID-USER-alice 4 users users PHID-REPO-git PHID-DIFF-new [] 40 \
             {\"draft.broadcast\":false,\"lines.added\":3,\"lines.removed\":1} Documents it.\n\n\
             See the guide."
        ]
    );
    assert_eq!(
        value(
            "SELECT CONCAT(revisionID, ' ', dateModified) AS value FROM \
             `{ns}_differential`.differential_diff WHERE id = 101"
        )
        .await,
        ["3 1700000000"]
    );
    let transactions = value(
        "SELECT CONCAT_WS(' ', transactionType, oldValue, newValue, metadata) AS value FROM \
         `{ns}_differential`.differential_transaction ORDER BY id",
    )
    .await;
    let types: Vec<&str> = transactions
        .iter()
        .map(|row| row.split(' ').next().unwrap_or_default())
        .collect();
    assert_eq!(
        types,
        [
            "core:create",
            "differential:update",
            "differential.revision.title",
            "differential.revision.summary",
            "differential.revision.testplan",
            "differential.revision.reviewers",
        ]
    );
    assert!(
        transactions[0]
            .starts_with("core:create null null {\"core.create\":true,\"core.groupID\":"),
        "{}",
        transactions[0]
    );
    assert!(
        transactions[1].starts_with("differential:update null \"PHID-DIFF-new\" {\"core.create\""),
        "{}",
        transactions[1]
    );
    // The new files' paths, newest changeset first, after the known root.
    assert_eq!(
        value(
            "SELECT CONCAT(a.repositoryID, ' ', p.path) AS value FROM \
             `{ns}_differential`.differential_affectedpath a JOIN \
             `{ns}_repository`.repository_path p ON p.id = a.pathID WHERE a.revisionID = 3 \
             ORDER BY p.id"
        )
        .await,
        ["1 /", "1 /docs/a.md", "1 /docs", "1 /README.md"]
    );
    assert_eq!(
        value(
            "SELECT CONCAT(type, ' ', TRIM(TRAILING '\\0' FROM hash)) AS value FROM \
             `{ns}_differential`.differential_revisionhash WHERE revisionID = 3 ORDER BY type"
        )
        .await,
        ["gtcm aaa", "gttr ttt"]
    );
    assert_eq!(
        value(
            "SELECT CONCAT(viewerPHID, ' ', viewState) AS value FROM \
             `{ns}_differential`.differential_viewstate WHERE objectPHID LIKE 'PHID-DREV-%'"
        )
        .await,
        ["PHID-USER-alice {\"x\":1}"]
    );
    assert_eq!(
        value(
            "SELECT CONCAT(reviewerPHID, ' ', reviewerStatus) AS value FROM \
             `{ns}_differential`.differential_reviewer WHERE revisionPHID LIKE 'PHID-DREV-%' \
             AND revisionPHID NOT IN ('PHID-DREV-1', 'PHID-DREV-2')"
        )
        .await,
        ["PHID-USER-bob added"]
    );
    check_new_object_task(&test_db, "D3").await;
}

/// The last task is the publish task of the new revision `monogram`,
/// which mails no one.
async fn check_new_object_task(test_db: &TestDatabase, monogram: &str) {
    let task = values(
        test_db,
        "SELECT CAST(data AS CHAR) AS value FROM `{ns}_worker`.worker_taskdata ORDER BY id \
         DESC LIMIT 1",
    )
    .await;
    let data: Value = serde_json::from_str(&task[0]).expect("JSON task data");
    assert_eq!(data["state"]["isNewObject"], json!(true));
    let stamps = data["state"]["mailStamps"].as_array().expect("stamps");
    let stamp = |key: &str| {
        stamps
            .iter()
            .find(|stamp| stamp["key"] == key)
            .map(|stamp| stamp["value"].clone())
    };
    assert_eq!(stamp("monogram"), Some(json!(monogram)));
    assert_eq!(stamp("new"), Some(json!(true)));
    assert_eq!(data["state"]["mailToPHIDs"], json!([]));
}

#[tokio::test]
async fn updates_the_diff_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let params = json!({
        "objectIdentifier": "D1",
        "transactions": [{"type": "update", "value": "PHID-DIFF-new"}],
    });
    let result = answer(edit(&context, ALICE, params).await);
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(2));

    let value = |sql: &'static str| values(&test_db, sql);
    // Back to review; Bob's rejection is voided, Carol's sticky accept
    // stays, so the status stays.
    assert_eq!(
        value(
            "SELECT CONCAT_WS(' ', status, lineCount, repositoryPHID, activeDiffPHID, \
             properties) AS value FROM `{ns}_differential`.differential_revision WHERE id = 1"
        )
        .await,
        ["needs-review 4 PHID-REPO-git PHID-DIFF-new {\"lines.added\":3,\"lines.removed\":1}"]
    );
    assert_eq!(
        value(
            "SELECT CONCAT_WS(' ', transactionType, oldValue, newValue, metadata) AS value FROM \
             `{ns}_differential`.differential_transaction ORDER BY id"
        )
        .await
        .iter()
        .map(|row| row.split(" {").next().unwrap_or_default().to_owned())
        .collect::<Vec<_>>(),
        [
            "differential:update \"PHID-DIFF-1\" \"PHID-DIFF-new\"",
            "differential.revision.void false",
        ]
    );
    assert_eq!(
        value(
            "SELECT CONCAT(newValue, ' ', metadata) AS value FROM \
             `{ns}_differential`.differential_transaction WHERE transactionType = \
             'differential.revision.void'"
        )
        .await[0]
            .split(",\"core.groupID\"")
            .next(),
        Some("{\"5\":\"PHID-USER-bob\"} {\"void.force\":[]")
    );
    assert_eq!(
        value(
            "SELECT CONCAT(reviewerPHID, ' ', COALESCE(voidedPHID, '-')) AS value FROM \
             `{ns}_differential`.differential_reviewer ORDER BY id"
        )
        .await,
        ["PHID-USER-bob PHID-USER-alice", "PHID-USER-carol -"]
    );
    assert_eq!(
        value(
            "SELECT CAST(revisionID AS CHAR) AS value FROM `{ns}_differential`.differential_diff \
             WHERE id = 101"
        )
        .await,
        ["1"]
    );
    assert_eq!(
        value(
            "SELECT CAST(COUNT(*) AS CHAR) AS value FROM \
             `{ns}_differential`.differential_affectedpath WHERE revisionID = 1"
        )
        .await,
        ["4"]
    );
}

#[tokio::test]
async fn leaves_the_rest_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let update = |diff: &str| json!({"type": "update", "value": diff});
    let refused = [
        // Attached to another revision.
        json!({"objectIdentifier": "D1", "transactions": [update("PHID-DIFF-2")]}),
        // An update with a review.
        json!({"objectIdentifier": "D1", "transactions": [
            update("PHID-DIFF-new"), {"type": "accept", "value": true}]}),
        // Unknown diffs.
        json!({"objectIdentifier": "D1", "transactions": [update("PHID-DIFF-none")]}),
        // A summary PHP reads as a commit message field.
        json!({"objectIdentifier": "D1", "transactions": [
            {"type": "summary", "value": "Fixes it.\nTest Plan: none"}]}),
        // New revisions without a test plan, a diff or a title.
        json!({"transactions": [update("PHID-DIFF-new"), {"type": "title", "value": "T"}]}),
        json!({"transactions": [{"type": "title", "value": "T"},
            {"type": "testPlan", "value": "P"}]}),
        json!({"transactions": [update("PHID-DIFF-new"), {"type": "testPlan", "value": "P"}]}),
        // Mentions and references in new revisions, and comments.
        creation(&[json!({"type": "comment", "value": "Hi"})]),
        json!({"transactions": [update("PHID-DIFF-new"), {"type": "title", "value": "T"},
            {"type": "testPlan", "value": "Ask @bob, see D2."}]}),
    ];
    for params in refused {
        assert!(
            matches!(
                edit(&context, ALICE, params.clone()).await,
                Response::Fallback(_)
            ),
            "{params}"
        );
    }
    // Bob can't see Alice's diff.
    assert!(matches!(
        edit(&context, BOB, creation(&[])).await,
        Response::Fallback(_)
    ));
    assert_eq!(
        values(
            &test_db,
            "SELECT CAST(COUNT(*) AS CHAR) AS value FROM \
             `{ns}_differential`.differential_transaction"
        )
        .await,
        ["0"]
    );
}
