//! Mozilla's AI change transactions of `differential.revision.edit`
//! (Bug 2091563) against a test database, after
//! `DifferentialAIChangeTestCase`.

// The fixtures build JSON values.
#![allow(clippy::needless_pass_by_value)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-aiaiaiaiaiaiaiaiaiaiaiaiaiai";
const NOW: i64 = 1_700_000_000;

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
];

async fn edit(context: &Context, params: Value) -> Response {
    call_method(context, "differential.revision.edit", params).await
}

async fn call_method(context: &Context, method: &str, params: Value) -> Response {
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
    call::dispatch(context, &registry, method, &http, Mode::Serve, NOW)
        .await
        .response
}

fn transactions(id: i64, transactions: Value) -> Value {
    json!({"objectIdentifier": format!("D{id}"), "transactions": transactions})
}

fn request(value: Value) -> Value {
    transactions(1, json!([{"type": "ai-change.request", "value": value}]))
}

/// D1 (alice's) has diffs 10, then 11, active; D2 has diff 2; D3 is
/// abandoned. Bob makes the edits.
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
             (1, 'Uplift', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'users', NULL, '{{\"draft.broadcast\":true}}', \
               'PHID-DIFF-11'), \
             (2, 'Other', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-2'), \
             (3, 'Abandoned', 'PHID-DREV-3', 'abandoned', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-3'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy) VALUES \
             (10, 'PHID-DIFF-10', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (11, 'PHID-DIFF-11', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (2, 'PHID-DIFF-2', 2, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (3, 'PHID-DIFF-3', 3, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users')"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn texts(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| phab_db::rows::text(row, "value").expect("a value"))
        .collect()
}

async fn properties(test_db: &TestDatabase, id: i64) -> Value {
    let sql = format!(
        "SELECT properties AS value FROM `{{ns}}_differential`.differential_revision WHERE id = {id}"
    );
    serde_json::from_str(&texts(test_db, &sql).await[0]).expect("JSON")
}

fn answered(response: &Response) -> bool {
    matches!(
        response,
        Response::Envelope(Envelope {
            error_code: None,
            ..
        })
    )
}

#[tokio::test]
async fn requests_an_approval_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let response = edit(
        &context,
        request(json!({
            "confidence": "high",
            "changeDiffID": "11",
            "baseDiffID": 10,
            "kind": "merge-conflict",
            "summary": "Kept the beta API.",
        })),
    )
    .await;
    assert!(answered(&response), "{response:?}");

    let new = json!({
        "kind": "merge-conflict", "changeDiffID": 11, "changeDiffPHID": "PHID-DIFF-11",
        "baseDiffID": 10, "summary": "Kept the beta API.", "confidence": "high", "details": null,
    });
    let rows = texts(
        &test_db,
        "SELECT CONCAT(transactionType, ' ', oldValue, ' ', newValue) AS value FROM \
         `{ns}_differential`.differential_transaction",
    )
    .await;
    assert_eq!(
        rows,
        vec![format!(
            "mozilla.ai-change.request null {}",
            phab_core::php::json_encode(&new)
        )]
    );
    let mut stored = new.as_object().unwrap().clone();
    stored.insert("requesterPHID".to_owned(), json!("PHID-USER-bob"));
    stored.insert("requestEpoch".to_owned(), json!(NOW));
    stored.insert("approverPHID".to_owned(), Value::Null);
    stored.insert("approvalEpoch".to_owned(), Value::Null);
    let properties = properties(&test_db, 1).await;
    assert_eq!(properties["draft.broadcast"], json!(true));
    assert_eq!(properties["mozilla.ai-change"], Value::Object(stored));
    assert_eq!(
        texts(
            &test_db,
            "SELECT taskClass AS value FROM `{ns}_worker`.worker_activetask"
        )
        .await,
        vec!["PhabricatorApplicationTransactionPublishWorker"]
    );

    // transaction.search shows the request's new value.
    let Response::Envelope(Envelope {
        result,
        error_code: None,
        ..
    }) = call_method(
        &context,
        "transaction.search",
        json!({"objectIdentifier": "D1"}),
    )
    .await
    else {
        panic!("expected an answer");
    };
    assert_eq!(result["data"][0]["type"], "ai-change.request");
    assert_eq!(result["data"][0]["fields"], new);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_refused_requests_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let change = 11;
    let cases = [
        json!("yes"),
        json!([]),
        json!({"kind": null, "changeDiffID": change}),
        json!({"kind": "vibes", "changeDiffID": change}),
        json!({"kind": "merge-conflict", "summary": "Kept the beta API."}),
        json!({"kind": "merge-conflict", "changeDiffID": 10}),
        json!({"kind": "merge-conflict", "changeDiffID": 0}),
        json!({"kind": "merge-conflict", "changeDiffID": 11.0}),
        json!({"kind": "merge-conflict", "changeDiffID": change, "baseDiffID": 2}),
        json!({"kind": "merge-conflict", "changeDiffID": change, "baseDiffID": change}),
        json!({"kind": "merge-conflict", "changeDiffID": change, "baseDiffID": ""}),
        json!({"kind": "merge-conflict", "changeDiffID": change, "approver": "someone"}),
        json!({"kind": "merge-conflict", "changeDiffID": change, "summary": ["x"]}),
        json!({"kind": "merge-conflict", "changeDiffID": change, "summary": "a".repeat(1025)}),
        json!({"kind": "merge-conflict", "changeDiffID": change, "confidence": "a".repeat(65)}),
        json!({"kind": "merge-conflict", "changeDiffID": change, "details": "a".repeat(32769)}),
    ];
    for value in cases {
        let response = edit(&context, request(value.clone())).await;
        assert!(
            matches!(response, Response::Fallback(_)),
            "{value}: {response:?}"
        );
    }
    // `DifferentialAIChangeEditorExtension`: not with an update.
    let response = edit(
        &context,
        transactions(
            1,
            json!([
                {"type": "update", "value": "PHID-DIFF-10"},
                {"type": "ai-change.request", "value": {"kind": "merge-conflict", "changeDiffID": 11}},
            ]),
        ),
    )
    .await;
    assert!(matches!(response, Response::Fallback(_)), "{response:?}");
    assert!(
        properties(&test_db, 1)
            .await
            .get("mozilla.ai-change")
            .is_none()
    );

    // Text at the limits is accepted.
    let response = edit(
        &context,
        request(json!({
            "kind": "requested-edit", "changeDiffID": 11, "summary": "a".repeat(1024),
            "confidence": "a".repeat(64), "details": "a".repeat(32768),
        })),
    )
    .await;
    assert!(answered(&response), "{response:?}");
    let stored = &properties(&test_db, 1).await["mozilla.ai-change"];
    assert_eq!(stored["details"].as_str().map(str::len), Some(32768));
    assert_eq!(stored["kind"], "requested-edit");
    test_db.drop_databases().await;
}

fn approve(id: i64, value: Value) -> Value {
    transactions(id, json!([{"type": "ai-change.approve", "value": value}]))
}

/// Stores a pending request on `id`, for diff `diff`.
async fn store_request(test_db: &TestDatabase, id: i64, diff: i64) {
    let data = json!({
        "kind": "merge-conflict", "changeDiffID": diff,
        "changeDiffPHID": format!("PHID-DIFF-{diff}"), "baseDiffID": null, "summary": null,
        "confidence": null, "details": null, "requesterPHID": "PHID-USER-lando",
        "requestEpoch": 1_600_000_000, "approverPHID": null, "approvalEpoch": null,
    });
    let properties = json!({"draft.broadcast": true, "mozilla.ai-change": data});
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_differential`.differential_revision SET properties = \
             '{properties}' WHERE id = {id}"
        ))
        .await;
}

#[tokio::test]
async fn approves_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    store_request(&test_db, 1, 11).await;
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
             reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-bob', 'added', 1, 1, '{}')",
        )
        .await;
    // With "Accept Revision", in one submission.
    let response = edit(
        &context,
        transactions(
            1,
            json!([
                {"type": "ai-change.approve", "value": 11},
                {"type": "accept", "value": true},
            ]),
        ),
    )
    .await;
    assert!(answered(&response), "{response:?}");
    let rows = texts(
        &test_db,
        "SELECT CONCAT(transactionType, ' ', oldValue, ' ', newValue) AS value FROM \
         `{ns}_differential`.differential_transaction ORDER BY id",
    )
    .await;
    assert_eq!(rows[0], "mozilla.ai-change.approve null 11");
    assert_eq!(rows[1], "differential.revision.accept false true");
    let stored = &properties(&test_db, 1).await["mozilla.ai-change"];
    assert_eq!(stored["approverPHID"], "PHID-USER-bob");
    assert_eq!(stored["approvalEpoch"], json!(NOW));
    assert_eq!(stored["requesterPHID"], "PHID-USER-lando");
    // The keys keep their place.
    assert_eq!(
        stored
            .as_object()
            .unwrap()
            .keys()
            .next_back()
            .map(String::as_str),
        Some("approvalEpoch")
    );

    let Response::Envelope(Envelope {
        result,
        error_code: None,
        ..
    }) = call_method(
        &context,
        "transaction.search",
        json!({"objectIdentifier": "D1"}),
    )
    .await
    else {
        panic!("expected an answer");
    };
    let approval = result["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|xaction| xaction["type"] == "ai-change.approve")
        .expect("the approval");
    assert_eq!(approval["fields"], json!({"diffID": 11}));
    assert_eq!(approval["authorPHID"], "PHID-USER-bob");

    // Approved already.
    let response = edit(&context, approve(1, json!(11))).await;
    assert!(matches!(response, Response::Fallback(_)), "{response:?}");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_refused_approvals_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // No request on D2; D3 is closed.
    store_request(&test_db, 3, 3).await;
    // D1's request is for a diff which isn't active any more.
    store_request(&test_db, 1, 10).await;
    for (id, value) in [
        (2, json!(2)),
        (3, json!(3)),
        (1, json!(10)),
        (1, json!(11)),
        (1, json!(true)),
        (1, Value::Null),
    ] {
        let response = edit(&context, approve(id, value.clone())).await;
        assert!(
            matches!(response, Response::Fallback(_)),
            "D{id} {value}: {response:?}"
        );
    }
    // A stale page approves the diff it saw.
    store_request(&test_db, 1, 11).await;
    let response = edit(&context, approve(1, json!(10))).await;
    assert!(matches!(response, Response::Fallback(_)), "{response:?}");
    // Bots can't approve.
    test_db
        .execute("UPDATE `{ns}_user`.user SET isSystemAgent = 1 WHERE phid = 'PHID-USER-bob'")
        .await;
    let response = edit(&context, approve(1, json!(11))).await;
    assert!(matches!(response, Response::Fallback(_)), "{response:?}");
    test_db
        .execute("UPDATE `{ns}_user`.user SET isSystemAgent = 0 WHERE phid = 'PHID-USER-bob'")
        .await;
    let response = edit(&context, approve(1, json!(11))).await;
    assert!(answered(&response), "{response:?}");
    test_db.drop_databases().await;
}
