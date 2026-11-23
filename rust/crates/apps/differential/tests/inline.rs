//! Inline comments against a test database: `differential.createinline`,
//! the `inline` transactions of `differential.revision.edit`, and the
//! drafts the comment form publishes.

use phab_app_differential::edit::{self, Source};
use phab_conduit::{ApiRequest, Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::{Config, Phid};
use phab_db::testing::TestDatabase;
use serde_json::{Map, Value, json};

const NOW: i64 = 1_700_000_000;
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
    "draft",
];

/// The comment of Alice Bob replies to.
const PARENT: &str = "PHID-XCMT-parent";

async fn call(context: &Context, method: &str, token: &str, params: Value) -> Response {
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
    call::dispatch(context, &registry, method, &http, Mode::Serve, NOW)
        .await
        .response
}

/// Bob and Alice; D1, Alice's, which Bob reviews, on diff 2 (`fruit`,
/// `basket`) after diff 1 (`fruit`), with Alice's published comment on
/// line 2 of `fruit`.
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
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Fruit', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-2'), \
             (2, 'Hidden', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'admin', 'admin', NULL, '{{}}', 'PHID-DIFF-3'); \
             INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, \
               reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-bob', 'added', 1, 1, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, repositoryPHID, sourceControlSystem, sourceControlPath, lintStatus, \
               unitStatus, lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', NULL, 'git', '/', 0, 0, 3, 1, 1, \
               'PHID-USER-alice'), \
             (2, 'PHID-DIFF-2', 1, 'PHID-USER-alice', NULL, 'git', '/', 0, 0, 4, 1, 1, \
               'PHID-USER-alice'), \
             (3, 'PHID-DIFF-3', 2, 'PHID-USER-alice', NULL, 'git', '/', 0, 0, 1, 1, 1, \
               'PHID-USER-alice'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, \
               newProperties, addLines, delLines, dateCreated, dateModified, phid) VALUES \
             (11, 1, NULL, 'fruit', '[]', 1, 1, '{{}}', '{{}}', '{{}}', 3, 0, 1, 1, \
               'PHID-DCNG-11'), \
             (21, 2, NULL, 'fruit', '[]', 1, 1, '{{}}', '{{}}', '{{}}', 3, 0, 1, 1, \
               'PHID-DCNG-21'), \
             (22, 2, 'basket', 'basket', '[]', 2, 1, '{{}}', '{{}}', '{{}}', 1, 1, 1, 1, \
               'PHID-DCNG-22'); \
             INSERT INTO `{{ns}}_differential`.differential_transaction_comment (id, phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, revisionPHID, changesetID, \
               isNewFile, lineNumber, lineLength, fixedState, hasReplies, replyToCommentPHID, \
               attributes) VALUES \
             (5, '{PARENT}', 'PHID-XACT-DREV-old', 'PHID-USER-alice', 'public', \
               'PHID-USER-alice', 1, 'Spelled right?', '{{}}', 0, 1, 1, 'PHID-DREV-1', 21, 1, 2, \
               1, 'done', 0, NULL, '{{\"editing\":false,\"x\":{{}}}}')"
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
        panic!("expected an answer: {response:?}");
    };
    result
}

fn error_code(response: Response) -> Option<String> {
    let Response::Envelope(Envelope { error_code, .. }) = response else {
        panic!("expected an envelope");
    };
    error_code
}

/// The comment rows after the published one, with their columns.
const COMMENTS: &str = "SELECT CONCAT_WS(' ', authorPHID, IFNULL(transactionPHID, '-'), \
    viewPolicy, editPolicy, commentVersion, content, contentSource, isDeleted, dateCreated, \
    dateModified, revisionPHID, changesetID, isNewFile, lineNumber, lineLength, \
    IFNULL(fixedState, '-'), hasReplies, IFNULL(replyToCommentPHID, '-'), attributes) AS value \
    FROM `{ns}_differential`.differential_transaction_comment WHERE id > 5 ORDER BY id";

#[tokio::test]
async fn creates_draft_inline_comments_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let params = json!({"revisionID": 1, "filePath": "fruit", "isNewFile": true,
        "lineNumber": 3, "lineLength": 0, "content": "Why?"});
    let result = answer(call(&context, "differential.createinline", BOB, params).await);
    assert_eq!(
        result,
        json!({"id": 6, "authorPHID": "PHID-USER-bob", "filePath": "fruit", "isNewFile": 1,
            "lineNumber": 3, "lineLength": 0, "diffID": "2", "content": "Why?"})
    );
    // On the old side of a file of diff 2, by its ID, with values as given.
    let params = json!({"diffID": "2", "filePath": "basket", "isNewFile": false,
        "lineNumber": "1", "content": "Pear?"});
    let result = answer(call(&context, "differential.createinline", BOB, params).await);
    assert_eq!(
        result,
        json!({"id": 7, "authorPHID": "PHID-USER-bob", "filePath": "basket", "isNewFile": 0,
            "lineNumber": "1", "lineLength": 0, "diffID": "2", "content": "Pear?"})
    );
    let rows = values(&test_db, COMMENTS).await;
    assert_eq!(
        rows,
        [
            "PHID-USER-bob - public PHID-USER-bob 1 Why? {\"source\":\"legacy\",\"params\":[]} 0 \
             1700000000 1700000000 PHID-DREV-1 21 1 3 0 - 0 - []",
            "PHID-USER-bob - public PHID-USER-bob 1 Pear? {\"source\":\"legacy\",\"params\":[]} \
             0 1700000000 1700000000 PHID-DREV-1 22 0 1 0 - 0 - []",
        ]
    );
    // No transaction, no task.
    assert_eq!(
        values(
            &test_db,
            "SELECT CAST(COUNT(*) AS CHAR) AS value FROM `{ns}_differential`.differential_transaction"
        )
        .await,
        ["0"]
    );

    for (params, code) in [
        (
            json!({"filePath": "fruit", "isNewFile": true, "lineNumber": 1, "content": "x"}),
            "ERR-NEED-DIFF",
        ),
        (
            json!({"revisionID": 9, "filePath": "fruit", "isNewFile": true, "lineNumber": 1,
            "content": "x"}),
            "ERR-BAD-REVISION",
        ),
        (
            json!({"revisionID": 1, "diffID": 3, "filePath": "fruit", "isNewFile": true,
            "lineNumber": 1, "content": "x"}),
            "ERR-BAD-DIFF",
        ),
        (
            json!({"diffID": 9, "filePath": "fruit", "isNewFile": true, "lineNumber": 1,
            "content": "x"}),
            "ERR-BAD-DIFF",
        ),
        (
            json!({"revisionID": 1, "filePath": "", "isNewFile": true, "lineNumber": 1,
            "content": "x"}),
            "ERR-NEED-FILE",
        ),
        (
            json!({"revisionID": 1, "filePath": "apple", "isNewFile": true, "lineNumber": 1,
            "content": "x"}),
            "ERR-BAD-FILE",
        ),
    ] {
        let response = call(&context, "differential.createinline", BOB, params.clone()).await;
        assert_eq!(error_code(response).as_deref(), Some(code), "{params}");
    }
    // Hidden revisions raise a policy exception.
    let params = json!({"revisionID": 2, "filePath": "x", "isNewFile": true, "lineNumber": 1,
        "content": "x"});
    assert!(matches!(
        call(&context, "differential.createinline", BOB, params).await,
        Response::Fallback(_)
    ));
}

/// The transactions of D1: type, comment version, old and new values and
/// metadata without the group ID.
async fn transactions(test_db: &TestDatabase) -> Vec<String> {
    values(
        test_db,
        "SELECT CONCAT_WS(' ', transactionType, commentVersion, oldValue, newValue, \
           contentSource, REPLACE(metadata, SUBSTRING(metadata, LOCATE('core.groupID', metadata) \
           + 15, 32), 'G')) AS value \
         FROM `{ns}_differential`.differential_transaction ORDER BY id",
    )
    .await
}

#[tokio::test]
async fn publishes_inline_transactions_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let params = json!({"objectIdentifier": "D1", "transactions": [
        {"type": "inline", "value": {"path": "fruit", "line": 2, "length": 2,
            "content": "Typo?", "suggestionText": "banana"}},
        {"type": "comment", "value": "See inline."},
        {"type": "inline", "value": {"replyToCommentPHID": PARENT, "content": "Agreed."}},
    ]});
    let result = answer(call(&context, "differential.revision.edit", BOB, params).await);
    let phids: Vec<&str> = result["transactions"]
        .as_array()
        .expect("transactions")
        .iter()
        .filter_map(|xaction| xaction["phid"].as_str())
        .collect();
    assert_eq!(phids.len(), 3);
    let conduit = r#"{"source":"conduit","params":[]}"#;
    // The comment first, then the inline comments (`sortTransactions()`).
    assert_eq!(
        transactions(&test_db).await,
        [
            format!(r#"core:comment 1 null null {conduit} {{"core.groupID":"G"}}"#),
            format!(r#"differential:inline 1 null null {conduit} {{"core.groupID":"G"}}"#),
            format!(r#"differential:inline 1 null null {conduit} {{"core.groupID":"G"}}"#),
        ]
    );
    assert_eq!(
        values(
            &test_db,
            "SELECT CONCAT_WS(' ', phid, commentPHID) AS value FROM \
             `{ns}_differential`.differential_transaction ORDER BY id"
        )
        .await
        .iter()
        .map(|row| row.split(' ').next().unwrap_or_default().to_owned())
        .collect::<Vec<_>>(),
        phids
    );
    let rows = values(&test_db, COMMENTS).await;
    assert_eq!(rows.len(), 3);
    assert!(rows[0].contains("See inline."), "{}", rows[0]);
    assert_eq!(
        rows[1]
            .split_once(' ')
            .map(|(_, rest)| rest.replace(phids[1], "X")),
        Some(format!(
            "X public PHID-USER-bob 1 Typo? {conduit} 0 1700000000 1700000000 PHID-DREV-1 21 1 \
             2 1 - 0 - {{\"inline.state\":{{\"text\":\"Typo?\",\"hasSuggestion\":true,\
             \"suggestionText\":\"banana\\n\"}},\"editing\":false}}"
        ))
    );
    // A reply goes on the lines of its parent.
    assert_eq!(
        rows[2]
            .split_once(' ')
            .map(|(_, rest)| rest.replace(phids[2], "X")),
        Some(format!(
            "X public PHID-USER-bob 1 Agreed. {conduit} 0 1700000000 1700000000 PHID-DREV-1 21 \
             1 2 1 - 0 {PARENT} {{\"inline.state\":{{\"text\":\"Agreed.\",\"hasSuggestion\":\
             false,\"suggestionText\":\"\"}},\"editing\":false}}"
        ))
    );
    // The parent is replied to, and saved again.
    assert_eq!(
        values(
            &test_db,
            "SELECT CONCAT_WS(' ', hasReplies, dateModified, attributes) AS value FROM \
             `{ns}_differential`.differential_transaction_comment WHERE id = 5"
        )
        .await,
        ["1 1700000000 {\"editing\":false,\"x\":[]}"]
    );
    assert_eq!(
        values(
            &test_db,
            "SELECT lastCommentDiffPHID AS value FROM `{ns}_differential`.differential_reviewer"
        )
        .await,
        ["PHID-DIFF-2"]
    );
    let data = values(
        &test_db,
        "SELECT data AS value FROM `{ns}_worker`.worker_taskdata",
    )
    .await;
    let data: Value = serde_json::from_str(&data[0]).expect("JSON");
    assert_eq!(data["xactionPHIDs"], json!(phids));
}

#[tokio::test]
async fn the_authors_inline_comments_start_done() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // Form-encoded values, on diff 1, alone.
    let params = json!({"objectIdentifier": "D1", "transactions": [
        {"type": "inline", "value": {"diff": "1", "path": "fruit", "line": "3",
            "isNewFile": "false", "content": "Old side."}},
    ]});
    let result = answer(call(&context, "differential.revision.edit", ALICE, params).await);
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(1));
    let rows = values(&test_db, COMMENTS).await;
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].contains(" PHID-DREV-1 11 0 3 0 done 0 - "),
        "{}",
        rows[0]
    );

    // What PHP refuses or does more of is left to it.
    for value in [
        json!({"path": "apple", "line": 1, "content": "x"}),
        json!({"diff": 3, "path": "fruit", "line": 1, "content": "x"}),
        json!({"replyToCommentPHID": "PHID-XCMT-nope", "content": "x"}),
        json!({"replyToCommentPHID": PARENT, "line": 2, "content": "x"}),
        json!({"path": "fruit", "line": 1}),
    ] {
        let params = json!({"objectIdentifier": "D1",
            "transactions": [{"type": "inline", "value": value}]});
        assert!(
            matches!(
                call(&context, "differential.revision.edit", ALICE, params).await,
                Response::Fallback(_)
            ),
            "{value}"
        );
    }
    let creation = json!({"transactions": [
        {"type": "update", "value": "PHID-DIFF-9"},
        {"type": "title", "value": "New"},
        {"type": "inline", "value": {"path": "fruit", "line": 1, "content": "x"}},
    ]});
    assert!(matches!(
        call(&context, "differential.revision.edit", ALICE, creation).await,
        Response::Fallback(_)
    ));
    assert_eq!(values(&test_db, COMMENTS).await.len(), 1);
}

/// Bob's draft request to edit D1 with `transactions`, as the comment form
/// makes it.
async fn publish_drafts(context: &Context, transactions: Value) -> Result<Value, String> {
    let user =
        phab_auth::user::load_users_by_phid(&context.database, &[Phid::new("PHID-USER-bob")])
            .await
            .expect("users")
            .pop()
            .expect("Bob");
    let mut params = Map::new();
    params.insert("objectIdentifier".to_owned(), json!(1));
    params.insert("transactions".to_owned(), transactions);
    let request = ApiRequest {
        context,
        viewer: user.viewer(),
        user: Some(user),
        params,
        strictly_typed: true,
        now: NOW,
        writes: phab_auth::Writes::Allowed,
    };
    let source = Source::Web {
        remarkup_control: json!([]),
    };
    edit::apply_with_drafts(&request, None, source)
        .await
        .map_err(|error| format!("{error:?}"))
}

#[tokio::test]
async fn the_comment_form_publishes_drafts_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // An empty comment and no draft: no effect, which PHP explains.
    let empty = json!([{"type": "comment", "value": ""}]);
    assert_eq!(
        publish_drafts(&context, empty.clone()).await,
        Err("Unsupported(\"empty comments\")".to_owned())
    );
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_transaction_comment (id, phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, revisionPHID, changesetID, \
               isNewFile, lineNumber, lineLength, fixedState, hasReplies, replyToCommentPHID, \
               attributes) VALUES \
             (6, 'PHID-XCMT-reply', NULL, 'PHID-USER-bob', 'public', 'PHID-USER-bob', 1, \
               'Agreed.', '{{}}', 0, 2, 2, 'PHID-DREV-1', 21, 1, 2, 1, NULL, 0, '{PARENT}', \
               '{{\"editing\":false,\"inline.state\":{{\"text\":\"Old\",\"hasSuggestion\":1,\
               \"suggestionText\":\"pear\\\\n\"}}}}'), \
             (7, 'PHID-XCMT-void', NULL, 'PHID-USER-bob', 'public', 'PHID-USER-bob', 1, '', \
               '{{}}', 0, 2, 2, 'PHID-DREV-1', 21, 1, 1, 0, NULL, 0, NULL, '[]'), \
             (8, 'PHID-XCMT-draft', NULL, 'PHID-USER-bob', 'public', 'PHID-USER-bob', 1, \
               'Pear?', '{{}}', 0, 2, 2, 'PHID-DREV-1', 22, 0, 1, 0, NULL, 0, NULL, '[]'), \
             (9, 'PHID-XCMT-alice', NULL, 'PHID-USER-alice', 'public', 'PHID-USER-alice', 1, \
               'Mine.', '{{}}', 0, 2, 2, 'PHID-DREV-1', 22, 0, 1, 0, NULL, 0, NULL, '[]'); \
             INSERT INTO `{{ns}}_draft`.draft_versioneddraft (objectPHID, authorPHID, version, \
               properties, dateCreated, dateModified) VALUES \
             ('PHID-XCMT-draft', 'PHID-USER-bob', 1, '{{}}', 1, 1), \
             ('PHID-XCMT-draft', 'PHID-USER-alice', 1, '{{}}', 1, 1)"
        ))
        .await;
    let result = publish_drafts(&context, empty).await.expect("published");
    // No comment: the drafts, by ID.
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(2));
    let web = r#"{"source":"web","params":[]}"#;
    assert_eq!(
        transactions(&test_db).await,
        [
            format!(r#"differential:inline 1 null null {web} {{"core.groupID":"G"}}"#),
            format!(r#"differential:inline 1 null null {web} {{"core.groupID":"G"}}"#),
        ]
    );
    let rows = values(
        &test_db,
        "SELECT CONCAT_WS(' ', id, IFNULL(transactionPHID, '-') <> '-', contentSource, \
         dateCreated, dateModified, attributes) AS value FROM \
         `{ns}_differential`.differential_transaction_comment WHERE id > 5 ORDER BY id",
    )
    .await;
    assert_eq!(
        rows,
        [
            format!(
                "6 1 {web} 2 1700000000 {{\"editing\":false,\"inline.state\":{{\"text\":\
                 \"Agreed.\",\"hasSuggestion\":true,\"suggestionText\":\"pear\\n\"}}}}"
            ),
            // Empty drafts stay drafts, as do those of others.
            "7 0 {} 2 2 []".to_owned(),
            format!(
                "8 1 {web} 2 1700000000 {{\"inline.state\":{{\"text\":\"Pear?\",\
                 \"hasSuggestion\":false,\"suggestionText\":\"\"}},\"editing\":false}}"
            ),
            "9 0 {} 2 2 []".to_owned(),
        ]
    );
    assert_eq!(
        values(
            &test_db,
            "SELECT authorPHID AS value FROM `{ns}_draft`.draft_versioneddraft"
        )
        .await,
        ["PHID-USER-alice"]
    );
    assert_eq!(
        values(
            &test_db,
            "SELECT CAST(hasReplies AS CHAR) AS value FROM \
             `{ns}_differential`.differential_transaction_comment WHERE id = 5"
        )
        .await,
        ["1"]
    );
}

#[tokio::test]
async fn drafts_being_edited_go_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // An empty draft being edited stays a draft; one with text gets a
    // warning from PHP.
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_transaction_comment (id, phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, revisionPHID, changesetID, \
               isNewFile, lineNumber, lineLength, fixedState, hasReplies, replyToCommentPHID, \
               attributes) VALUES \
             (7, 'PHID-XCMT-void', NULL, 'PHID-USER-bob', 'public', 'PHID-USER-bob', 1, '', \
               '{}', 0, 2, 2, 'PHID-DREV-1', 21, 1, 1, 0, NULL, 0, NULL, '{\"editing\":true}')",
        )
        .await;
    let comment = json!([{"type": "comment", "value": "Again."}]);
    let result = publish_drafts(&context, comment.clone())
        .await
        .expect("a comment");
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(1));
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_transaction_comment SET content = 'Hm' \
             WHERE id = 7",
        )
        .await;
    assert_eq!(
        publish_drafts(&context, comment).await,
        Err("Unsupported(\"inline comments being edited\")".to_owned())
    );
}
