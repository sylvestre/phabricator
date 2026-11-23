//! The editor's closing, inline states, buildable statuses and custom
//! fields against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

/// Alice's token.
const ALICE: &str = "api-aaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Bob's token.
const BOB: &str = "api-bbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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
    "draft",
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

async fn edit(context: &Context, token: &str, params: Value) -> Response {
    invoke(context, "differential.revision.edit", token, params).await
}

/// Alice, Bob (an administrator) and the Harbormaster application; D1,
/// Alice's, accepted by Bob on diff 1, with an inline comment of Bob's
/// marked done in a draft; D2, Alice's, needing review, on diff 2.
async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-alice', 'api', '{ALICE}', NULL, 1, 1), \
             ('PHID-USER-bob', 'api', '{BOB}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Accepted', 'PHID-DREV-1', 'accepted', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{\"lines.added\":3,\"lines.removed\":1}}', \
               'PHID-DIFF-1'), \
             (2, 'Open', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '[]', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, repositoryPHID, sourceControlSystem, sourceControlPath, lintStatus, \
               unitStatus, lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', NULL, 'git', '/', 0, 0, 4, 1, 1, 'users'), \
             (2, 'PHID-DIFF-2', 2, 'PHID-USER-alice', NULL, 'git', '/', 0, 0, 4, 1, 1, 'users'), \
             (101, 'PHID-DIFF-new', NULL, 'PHID-USER-alice', 'PHID-REPO-git', 'git', '/', 0, \
               0, 2, 1, 1, 'users'); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-git', 'Git', NULL, 'git', '{{}}', 1, 1, 'users', 'admin', 'admin'); \
             INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, \
               reviewerPHID, reviewerStatus, lastActionDiffPHID, dateCreated, dateModified, \
               options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-bob', 'accepted', 'PHID-DIFF-1', 1, 1, '{{}}'), \
             ('PHID-DREV-2', 'PHID-USER-bob', 'added', NULL, 1, 1, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, diffID, filename, \
               changeType, fileType, metadata, oldProperties, newProperties, addLines, delLines, \
               dateCreated, dateModified, phid) VALUES \
             (1, 1, 'README', 2, 1, '[]', '[]', '[]', 3, 1, 1, 1, 'PHID-DCNG-1'), \
             (2, 101, 'README', 2, 1, '[]', '[]', '[]', 1, 1, 1, 1, 'PHID-DCNG-2'); \
             INSERT INTO `{{ns}}_differential`.differential_transaction_comment (phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, revisionPHID, changesetID, \
               isNewFile, lineNumber, lineLength, fixedState, hasReplies, replyToCommentPHID, \
               attributes) VALUES \
             ('PHID-XCMT-bob', 'PHID-XACT-DREV-old', 'PHID-USER-bob', 'public', 'PHID-USER-bob', \
               1, 'Typo.', '{{}}', 0, 1, 1, 'PHID-DREV-1', 1, 1, 2, 0, 'draft', 0, NULL, '[]'), \
             ('PHID-XCMT-own', NULL, 'PHID-USER-alice', 'public', 'PHID-USER-alice', 1, \
               'Mine.', '{{}}', 0, 1, 1, 'PHID-DREV-1', 1, 1, 3, 0, 'undraft', 0, NULL, '[]'), \
             ('PHID-XCMT-void', NULL, 'PHID-USER-alice', 'public', 'PHID-USER-alice', 1, '', \
               '{{}}', 0, 1, 1, 'PHID-DREV-1', 1, 1, 4, 0, 'draft', 0, NULL, '[]')"
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

async fn rows(test_db: &TestDatabase, sql: &str) -> Vec<Vec<Option<String>>> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| {
            use phab_db::sqlx::{Column, Row};
            row.columns()
                .iter()
                .map(|column| phab_db::rows::opt_text(row, column.name()).expect("text"))
                .collect()
        })
        .collect()
}

fn answered(response: Response) -> Value {
    match response {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("expected an answer, got {other:?}"),
    }
}

/// The transaction types of the revision, in their order, with their old
/// and new values and their metadata without the group.
async fn transactions(test_db: &TestDatabase, revision: &str) -> Vec<Vec<Option<String>>> {
    let mut found = rows(
        test_db,
        &format!(
            "SELECT transactionType, oldValue, newValue, metadata, authorPHID, editPolicy, \
             contentSource FROM `{{ns}}_differential`.differential_transaction WHERE objectPHID \
             = '{revision}' ORDER BY id"
        ),
    )
    .await;
    for row in &mut found {
        if let Some(metadata) = &mut row[3] {
            let mut value: Value = serde_json::from_str(metadata).expect("JSON");
            if let Value::Object(map) = &mut value {
                map.remove("core.groupID");
            }
            *metadata = value.to_string();
        }
    }
    found
}

/// A non-null column's value.
#[allow(clippy::unnecessary_wraps)]
fn text(value: &str) -> Option<String> {
    Some(value.to_owned())
}

#[tokio::test]
async fn closes_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let result = answered(
        edit(
            &context,
            ALICE,
            json!({"objectIdentifier": "D1", "transactions": [{"type": "close", "value": true}]}),
        )
        .await,
    );
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(1));
    let found = transactions(&test_db, "PHID-DREV-1").await;
    assert_eq!(
        found,
        [vec![
            text("differential.revision.close"),
            text("false"),
            text("true"),
            text("{}"),
            text("PHID-USER-alice"),
            text("PHID-USER-alice"),
            text(r#"{"source":"conduit","params":[]}"#),
        ]]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT status, properties FROM `{ns}_differential`.differential_revision WHERE id = 1"
        )
        .await,
        [vec![
            text("published"),
            text(
                r#"{"lines.added":3,"lines.removed":1,"wasAcceptedBeforeClose":true,"draft.broadcast":true}"#
            ),
        ]]
    );
    // The inline comments marked done in a draft are left as they are.
    assert_eq!(
        rows(
            &test_db,
            "SELECT fixedState FROM `{ns}_differential`.differential_transaction_comment \
             ORDER BY id"
        )
        .await,
        [
            vec![text("draft")],
            vec![text("undraft")],
            vec![text("draft")]
        ]
    );
    // Closing it again, or closing a revision which isn't accepted, is
    // refused by PHP; so is closing someone else's.
    for (token, id) in [(ALICE, "D1"), (ALICE, "D2"), (BOB, "D2")] {
        let params =
            json!({"objectIdentifier": id, "transactions": [{"type": "close", "value": true}]});
        assert!(
            matches!(edit(&context, token, params).await, Response::Fallback(_)),
            "{id}"
        );
    }
    test_db.drop_databases().await;
}

#[tokio::test]
async fn closes_with_differential_close() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let result = invoke(
        &context,
        "differential.close",
        ALICE,
        json!({"revisionID": 1}),
    )
    .await;
    assert_eq!(answered(result), Value::Null);
    assert_eq!(
        rows(
            &test_db,
            "SELECT status FROM `{ns}_differential`.differential_revision WHERE id = 1"
        )
        .await,
        [vec![text("published")]]
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn publishes_inline_states_with_a_comment() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let params = json!({"objectIdentifier": "D1", "transactions": [
        {"type": "comment", "value": "Fixed."},
        {"type": "title", "value": "Accepted, fixed"},
    ]});
    let result = answered(edit(&context, ALICE, params).await);
    let found = transactions(&test_db, "PHID-DREV-1").await;
    let kinds: Vec<_> = found.iter().map(|row| row[0].clone()).collect();
    // The state follows the comment, which goes last.
    assert_eq!(
        kinds,
        [
            text("core:inlinestate"),
            text("differential.revision.title"),
            text("core:comment"),
        ]
    );
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(3));
    // Alice's void draft isn't publishable; her other draft and Bob's
    // published comment are, newest first.
    assert_eq!(
        found[0][1..4],
        [
            text(r#"{"PHID-XCMT-own":"undraft","PHID-XCMT-bob":"draft"}"#),
            text(r#"{"PHID-XCMT-own":"undone","PHID-XCMT-bob":"done"}"#),
            text(
                r#"{"inline.details":{"PHID-XCMT-own":{"authorPHID":"PHID-USER-alice"},"PHID-XCMT-bob":{"authorPHID":"PHID-USER-bob"}}}"#
            ),
        ]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT fixedState FROM `{ns}_differential`.differential_transaction_comment \
             WHERE changesetID IS NOT NULL ORDER BY id"
        )
        .await,
        [
            vec![text("done")],
            vec![text("undone")],
            vec![text("draft")]
        ]
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_inline_states_alone_without_a_trigger() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // Bob isn't the author: only his own drafts would count, and he has
    // none; a title edit expands no inline state anyway.
    let params = json!({"objectIdentifier": "D1", "transactions": [
        {"type": "comment", "value": "Thanks."},
    ]});
    answered(edit(&context, BOB, params).await);
    let params = json!({"objectIdentifier": "D1", "transactions": [
        {"type": "title", "value": "Retitled"},
    ]});
    answered(edit(&context, ALICE, params).await);
    let kinds: Vec<_> = transactions(&test_db, "PHID-DREV-1")
        .await
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    assert!(!kinds.contains(&text("core:inlinestate")), "{kinds:?}");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn sets_custom_fields_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // Bob is an administrator: BMO isn't asked.
    let params = json!({"objectIdentifier": "D2", "transactions": [
        {"type": "bugzilla.bug-id", "value": "1002"},
    ]});
    answered(edit(&context, BOB, params.clone()).await);
    let found = transactions(&test_db, "PHID-DREV-2").await;
    assert_eq!(
        found,
        [vec![
            text("core:customfield"),
            text("null"),
            text("\"1002\""),
            text(r#"{"customfield:key":"differential:bugzilla-bug-id"}"#),
            text("PHID-USER-bob"),
            text("PHID-USER-bob"),
            text(r#"{"source":"conduit","params":[]}"#),
        ]]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT objectPHID, fieldValue FROM \
             `{ns}_differential`.differential_customfieldstorage"
        )
        .await,
        [vec![text("PHID-DREV-2"), text("1002")]]
    );
    // The same bug again has no effect: PHP saves the revision anyway.
    assert!(matches!(
        edit(&context, BOB, params).await,
        Response::Fallback(_)
    ));
    // Alice isn't an administrator: PHP asks BMO.
    let params = json!({"objectIdentifier": "D2", "transactions": [
        {"type": "bugzilla.bug-id", "value": "1003"},
    ]});
    assert!(matches!(
        edit(&context, ALICE, params).await,
        Response::Fallback(_)
    ));
    // The auditors are a JSON list.
    let params = json!({"objectIdentifier": "D2", "transactions": [
        {"type": "phabricator:auditors", "value": ["PHID-USER-bob"]},
    ]});
    answered(edit(&context, ALICE, params).await);
    let found = transactions(&test_db, "PHID-DREV-2").await;
    assert_eq!(
        found[1][..4],
        [
            text("core:customfield"),
            text("\"[]\""),
            text(r#""[\"PHID-USER-bob\"]""#),
            text(r#"{"customfield:key":"phabricator:auditors"}"#),
        ]
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn updates_revisions_with_arc_fields() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // arc sends every field of the message: those which didn't change are
    // dropped, as the edit continues on no effect.
    let params = json!({"id": 2, "diffid": 101, "fields": {
        "title": "Open",
        "summary": "",
        "testPlan": "Ran it.",
        "reviewerPHIDs": ["PHID-USER-bob"],
        "ccPHIDs": [],
        "bugzilla.bug-id": "",
        "revisionID": 2,
    }, "message": "Update."});
    let result = invoke(&context, "differential.updaterevision", ALICE, params).await;
    assert_eq!(
        answered(result),
        json!({"revisionid": "2", "uri": "http://phab.test/D2"})
    );
    let kinds: Vec<_> = transactions(&test_db, "PHID-DREV-2")
        .await
        .into_iter()
        .map(|row| (row[0].clone(), row[2].clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            (text("differential:update"), text("\"PHID-DIFF-new\"")),
            (text("differential.revision.testplan"), text("\"Ran it.\"")),
            (text("core:customfield"), text("\"\"")),
            (text("core:comment"), text("null")),
        ]
    );
    // A reviewer edit with effect, with the new diff, is still PHP's.
    let params = json!({"id": 1, "diffid": 101, "fields": {
        "reviewerPHIDs": [],
    }, "message": "Update."});
    assert!(matches!(
        invoke(&context, "differential.updaterevision", ALICE, params).await,
        Response::Fallback(_)
    ));
    test_db.drop_databases().await;
}

#[tokio::test]
async fn publishes_buildable_statuses() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let now = 1_700_000_000;
    let publish = |status| {
        phab_app_differential::edit::publish_buildable(
            &context,
            1,
            ("PHID-HMBB-1", status),
            (now, true),
        )
    };
    publish(Some("passed")).await.expect("published");
    let found = transactions(&test_db, "PHID-DREV-1").await;
    assert_eq!(
        found,
        [vec![
            text("harbormaster:buildable"),
            text("null"),
            text("\"passed\""),
            text(r#"{"harbormaster:buildablePHID":"PHID-HMBB-1"}"#),
            text("PHID-APPS-PhabricatorHarbormasterApplication"),
            text("no-one"),
            text(r#"{"source":"daemon","params":[]}"#),
        ]]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT status, properties FROM `{ns}_differential`.differential_revision WHERE id = 1"
        )
        .await,
        [vec![
            text("accepted"),
            text(
                r#"{"lines.added":3,"lines.removed":1,"buildables":{"PHID-HMBB-1":{"status":"passed"}}}"#
            ),
        ]]
    );
    // The same status again: nothing to publish.
    publish(Some("passed")).await.expect("nothing");
    assert_eq!(transactions(&test_db, "PHID-DREV-1").await.len(), 1);
    // A buildable of the active diff still preparing builds gets Herald's
    // message, which is PHP's.
    test_db
        .execute(
            "INSERT INTO `{ns}_harbormaster`.harbormaster_buildable (id, phid, buildablePHID, \
               containerPHID, buildableStatus, dateCreated, dateModified, isManualBuildable) \
             VALUES (2, 'PHID-HMBB-2', 'PHID-DIFF-1', 'PHID-DREV-1', 'preparing', 1, 1, 0)",
        )
        .await;
    assert!(publish(Some("failed")).await.is_err());
    test_db.drop_databases().await;
}
