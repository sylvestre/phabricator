//! `transaction.search` on revisions against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-tttttttttttttttttttttttttttt";

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
        "transaction.search",
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

async fn error(context: &Context, params: Value) -> (String, Option<String>) {
    match query(context, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info,
            ..
        }) => (code, error_info),
        other => panic!("{params}: expected an error, got {other:?}"),
    }
}

/// The IDs of the transactions of an answer, in order.
fn ids(result: &Value) -> Vec<i64> {
    result["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|xaction| xaction["id"].as_i64().expect("id"))
        .collect()
}

/// The transaction with this ID in an answer.
fn transaction(result: &Value, id: i64) -> &Value {
    result["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|xaction| xaction["id"] == id)
        .unwrap_or_else(|| panic!("no transaction {id} in {result}"))
}

/// A transaction row: `(id, type, author, revision, view policy, comment,
/// old, new, metadata)`.
type Row<'a> = (
    i64,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
);

async fn add_transactions(test_db: &TestDatabase, rows: &[Row<'_>]) {
    for (id, kind, author, revision, view, comment, old, new, metadata) in rows {
        let comment = if comment.is_empty() {
            "NULL".to_owned()
        } else {
            format!("'{comment}'")
        };
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_transaction (id, phid, authorPHID, \
                   objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, \
                   transactionType, oldValue, newValue, contentSource, metadata, dateCreated, \
                   dateModified) VALUES \
                 ({id}, 'PHID-XACT-DREV-{id}', '{author}', '{revision}', '{view}', '{author}', \
                   {comment}, 1, '{kind}', '{old}', '{new}', '{{}}', '{metadata}', {id}00, {id}01)"
            ))
            .await;
    }
}

/// A version of the comment of a transaction.
async fn add_comment(
    test_db: &TestDatabase,
    id: i64,
    transaction: i64,
    version: i64,
    content: &str,
    deleted: bool,
    inline: &str,
) {
    let deleted = i64::from(deleted);
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_transaction_comment (id, phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, changesetID, isNewFile, \
               lineNumber, lineLength, fixedState, hasReplies, replyToCommentPHID, attributes) \
             SELECT {id}, 'PHID-XCMT-{id}', 'PHID-XACT-DREV-{transaction}', 'PHID-USER-bob', \
               'users', 'PHID-USER-bob', {version}, '{content}', '{{}}', {deleted}, {id}0, {id}1, \
               {inline}"
        ))
        .await;
}

/// The transactions of the revisions.
const HISTORY: &[Row<'static>] = &[
    (
        1,
        "core:create",
        "PHID-USER-alice",
        "PHID-DREV-open",
        "users",
        "",
        "null",
        "true",
        "{}",
    ),
    (
        2,
        "differential.revision.title",
        "PHID-USER-alice",
        "PHID-DREV-open",
        "users",
        "",
        "\"\"",
        "\"Open\"",
        "{}",
    ),
    (
        3,
        "differential.revision.reviewers",
        "PHID-USER-alice",
        "PHID-DREV-open",
        "users",
        "",
        "[]",
        "{\"PHID-USER-bob\":\"blocking\"}",
        "{}",
    ),
    (
        4,
        "core:comment",
        "PHID-USER-bob",
        "PHID-DREV-open",
        "users",
        "PHID-XCMT-5",
        "null",
        "null",
        "{}",
    ),
    (
        5,
        "differential:inline",
        "PHID-USER-bob",
        "PHID-DREV-open",
        "users",
        "PHID-XCMT-6",
        "null",
        "null",
        "{}",
    ),
    (
        6,
        "core:edge",
        "PHID-USER-alice",
        "PHID-DREV-open",
        "users",
        "",
        "[]",
        "{\"PHID-PROJ-x\":{\"src\":\"PHID-DREV-open\",\"type\":41,\"dst\":\"PHID-PROJ-x\"}}",
        "{\"edge:type\":41,\"core.groupID\":\"g1\"}",
    ),
    (
        7,
        "differential:update",
        "PHID-USER-alice",
        "PHID-DREV-open",
        "users",
        "",
        "null",
        "\"PHID-DIFF-1\"",
        "{\"commitPHIDs\":[\"PHID-CMIT-1\"]}",
    ),
    (
        8,
        "core:customfield",
        "PHID-USER-alice",
        "PHID-DREV-open",
        "users",
        "",
        "1",
        "2",
        "{}",
    ),
    // Its own policy hides it from Bob.
    (
        9,
        "core:comment",
        "PHID-USER-alice",
        "PHID-DREV-open",
        "admin",
        "",
        "null",
        "null",
        "{}",
    ),
    // Its revision hides it from Bob.
    (
        10,
        "core:comment",
        "PHID-USER-alice",
        "PHID-DREV-hidden",
        "users",
        "",
        "null",
        "null",
        "{}",
    ),
    (
        11,
        "core:subscribers",
        "PHID-USER-bob",
        "PHID-DREV-open",
        "users",
        "",
        "[\"PHID-USER-carol\"]",
        "[\"PHID-USER-bob\"]",
        "{}",
    ),
];

/// The transactions of the revisions, and the comments of some.
async fn add_history(test_db: &TestDatabase) {
    add_transactions(test_db, HISTORY).await;
    let general = "NULL, 0, 0, 0, NULL, 0, NULL, '{}'";
    add_comment(test_db, 4, 4, 1, "First words", false, general).await;
    add_comment(test_db, 5, 4, 2, "", true, general).await;
    add_comment(
        test_db,
        6,
        5,
        1,
        "Use this",
        false,
        "1, 1, 10, 2, 'done', 0, 'PHID-XCMT-4', \
         '{\"inline.state\":{\"hasSuggestion\":true,\"suggestionText\":\"let x = 1;\"}}'",
    )
    .await;
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
               '{{}}', 'k', 'admin', 'admin', NULL, '{{}}', ''); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', 0, 0, 3, 1, 1, 'users'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, diffID, filename, \
               changeType, fileType, addLines, delLines, dateCreated, dateModified, phid) VALUES \
             (1, 1, 'src/main.rs', 2, 1, 3, 0, 1, 1, 'PHID-DCST-1'), \
             (2, 1, 'src', 2, 4, 0, 0, 1, 1, 'PHID-DCST-2')"
        ))
        .await;
    add_history(&test_db).await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn lists_what_the_viewer_can_see() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let by_name = answer(&context, json!({"objectIdentifier": "D1"})).await;
    assert_eq!(ids(&by_name), [11, 8, 7, 6, 5, 4, 3, 2, 1]);
    assert_eq!(
        by_name["cursor"],
        json!({"limit": 100, "after": null, "before": null})
    );
    let by_phid = answer(&context, json!({"objectIdentifier": "PHID-DREV-open"})).await;
    assert_eq!(by_phid, by_name);
    assert_eq!(
        answer(&context, json!({"objectIdentifier": "d1"})).await,
        by_name
    );
    // Every revision, without those Bob can't see.
    assert_eq!(
        ids(&answer(&context, json!({"objectType": "DREV"})).await),
        [11, 8, 7, 6, 5, 4, 3, 2, 1]
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn describes_each_transaction() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let result = answer(&context, json!({"objectIdentifier": "D1"})).await;

    let create = transaction(&result, 1);
    assert_eq!(create["type"], "create");
    assert_eq!(create["fields"], json!({}));
    assert_eq!(create["phid"], "PHID-XACT-DREV-1");
    assert_eq!(create["authorPHID"], "PHID-USER-alice");
    assert_eq!(create["objectPHID"], "PHID-DREV-open");
    assert_eq!(create["dateCreated"], 100);
    assert_eq!(create["dateModified"], 101);
    assert_eq!(create["groupID"], Value::Null);
    assert_eq!(create["comments"], json!([]));

    assert_eq!(transaction(&result, 2)["type"], "title");
    assert_eq!(
        transaction(&result, 2)["fields"],
        json!({"old": "", "new": "Open"})
    );
    assert_eq!(
        transaction(&result, 3)["fields"],
        json!({"operations": [{
            "operation": "add",
            "phid": "PHID-USER-bob",
            "oldStatus": null,
            "newStatus": "blocking",
            "isBlocking": true,
        }]})
    );
    assert_eq!(
        transaction(&result, 6),
        &json!({
            "id": 6,
            "phid": "PHID-XACT-DREV-6",
            "type": "projects",
            "authorPHID": "PHID-USER-alice",
            "objectPHID": "PHID-DREV-open",
            "dateCreated": 600,
            "dateModified": 601,
            "groupID": "g1",
            "comments": [],
            "fields": {"operations": [{"operation": "add", "phid": "PHID-PROJ-x"}]},
        })
    );
    assert_eq!(
        transaction(&result, 7)["fields"],
        json!({"old": null, "new": "PHID-DIFF-1", "commitPHIDs": ["PHID-CMIT-1"]})
    );
    assert_eq!(transaction(&result, 8)["type"], Value::Null);
    assert_eq!(transaction(&result, 8)["fields"], json!({}));
    assert_eq!(
        transaction(&result, 11)["fields"],
        json!({"operations": [
            {"operation": "add", "phid": "PHID-USER-bob"},
            {"operation": "remove", "phid": "PHID-USER-carol"},
        ]})
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn shows_comments_and_inlines() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let result = answer(&context, json!({"objectIdentifier": "D1"})).await;

    // The newest version first; once removed, no version shows its text.
    let comment = transaction(&result, 4);
    assert_eq!(comment["type"], "comment");
    assert_eq!(
        comment["comments"],
        json!([
            {"id": 5, "phid": "PHID-XCMT-5", "version": 2, "authorPHID": "PHID-USER-bob",
             "dateCreated": 50, "dateModified": 51, "removed": true, "content": {"raw": ""}},
            {"id": 4, "phid": "PHID-XCMT-4", "version": 1, "authorPHID": "PHID-USER-bob",
             "dateCreated": 40, "dateModified": 41, "removed": false, "content": {"raw": ""}},
        ])
    );

    let inline = transaction(&result, 5);
    assert_eq!(inline["type"], "inline");
    assert_eq!(inline["comments"][0]["content"], json!({"raw": "Use this"}));
    assert_eq!(
        inline["fields"],
        json!({
            "diff": {"id": 1, "phid": "PHID-DIFF-1"},
            "path": "src/main.rs",
            "line": 10,
            "length": 3,
            "isNewFile": true,
            "replyToCommentPHID": "PHID-XCMT-4",
            "isDone": true,
            "hasSuggestion": true,
            "suggestionText": "let x = 1;",
        })
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn pages_and_constrains() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let first = answer(&context, json!({"objectIdentifier": "D1", "limit": 3})).await;
    assert_eq!(ids(&first), [11, 8, 7]);
    assert_eq!(
        first["cursor"],
        json!({"limit": 3, "after": "7", "before": null})
    );
    let second = answer(
        &context,
        json!({"objectIdentifier": "D1", "limit": 3, "after": "7"}),
    )
    .await;
    assert_eq!(ids(&second), [6, 5, 4]);
    assert_eq!(
        second["cursor"],
        json!({"limit": 3, "after": "4", "before": "6"})
    );
    let back = answer(
        &context,
        json!({"objectIdentifier": "D1", "limit": 3, "before": "6"}),
    )
    .await;
    assert_eq!(ids(&back), [11, 8, 7]);
    // The limit is clamped between 1 and 1000.
    assert_eq!(
        ids(&answer(&context, json!({"objectIdentifier": "D1", "limit": 0})).await),
        [11]
    );

    assert_eq!(
        ids(&answer(
            &context,
            json!({"objectType": "DREV", "constraints": {"authorPHIDs": ["PHID-USER-bob"]}}),
        )
        .await),
        [11, 5, 4]
    );
    assert_eq!(
        ids(&answer(
            &context,
            json!({
                "objectIdentifier": "D1",
                "constraints": {"phids": ["PHID-XACT-DREV-2", "PHID-XACT-DREV-9"]},
            }),
        )
        .await),
        [2]
    );
    // PHP's empty array is a map without constraints.
    assert_eq!(
        ids(&answer(
            &context,
            json!({"objectIdentifier": "D1", "constraints": []})
        )
        .await)
        .len(),
        9
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn raises_php_errors() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let core = |message: &str| ("ERR-CONDUIT-CORE".to_owned(), Some(message.to_owned()));

    assert_eq!(
        error(&context, json!({})).await,
        core(
            "Calls to \"transaction.search\" must specify either an \"objectType\" or an \
             \"objectIdentifier\""
        )
    );
    assert_eq!(
        error(
            &context,
            json!({"objectIdentifier": "D1", "objectType": "DREV"})
        )
        .await,
        core(
            "Calls to \"transaction.search\" must not specify both an \"objectType\" and an \
             \"objectIdentifier\"."
        )
    );
    for name in ["D2", "D99", "PHID-DREV-nope"] {
        assert_eq!(
            error(&context, json!({"objectIdentifier": name})).await,
            core(&format!(
                "In call to \"transaction.search\", specified \"objectIdentifier\" (\"{name}\") \
                 does not exist."
            ))
        );
    }
    assert_eq!(
        error(
            &context,
            json!({"objectIdentifier": "D1", "constraints": {"authorPHIDs": []}})
        )
        .await,
        core(
            "Constraint \"authorPHIDs\" to \"transaction.search\" requires nonempty list, empty \
             list provided."
        )
    );
    assert_eq!(
        error(&context, json!({"objectType": "DREV", "after": "999"})).await,
        core(
            "Cursor \"999\" does not identify a valid object in query \"DifferentialTransactionQuery\"."
        )
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_the_rest_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    for params in [
        json!({"objectType": "TASK"}),
        json!({"objectIdentifier": "T1"}),
        json!({"objectIdentifier": "PHID-TASK-1"}),
        json!({"objectIdentifier": "D1", "constraints": {"bogus": ["x"]}}),
        json!({"objectIdentifier": "D1", "constraints": {"phids": null}}),
        // PHP raises its policy exception for these cursors.
        json!({"objectType": "DREV", "after": "9"}),
        json!({"objectType": "DREV", "after": "10"}),
    ] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}
