//! `diffusion.repository.edit` against a test database.
//!
//! Alice, who calls, edits repository 1; repository 2 has the callsign
//! `OTHER` and the short name `other`; she can see repository 3 but not
//! edit it.

// One fixture, many scenarios: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-rererererererererererererere";
const NOW: i64 = 1_700_000_000;

const SEED: &str = "\
    INSERT INTO `{ns}_user`.user (phid, userName, realName, dateCreated, dateModified, \
      conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
      accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
    ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
    INSERT INTO `{ns}_conduit`.conduit_token \
      (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
    ('PHID-USER-alice', 'api', 'TOKEN', NULL, 1, 1); \
    INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, repositorySlug, \
      versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
      pushPolicy, localPath, almanacServicePHID) VALUES \
    (1, 'PHID-REPO-one', 'One', 'ONE', NULL, 'git', \
      '{\"tracking-enabled\":true,\"default-branch\":\"main\",\"remote-uri\":\"a/b\"}', 1, 1, \
      'users', 'users', 'users', '/var/repo/1/', NULL), \
    (2, 'PHID-REPO-two', 'Two', 'OTHER', 'other', 'git', '[]', 1, 1, 'users', 'users', \
      'users', '/var/repo/2/', NULL), \
    (3, 'PHID-REPO-three', 'Three', 'THREE', NULL, 'git', '{}', 1, 1, 'users', 'admin', \
      'users', '/var/repo/3/', NULL); \
    INSERT INTO `{ns}_repository`.repository_statusmessage (repositoryID, statusType, \
      statusCode, parameters, epoch, messageCount) VALUES \
    (1, 'needs-update', 'okay', '[]', 5, 1), (1, 'fetch', 'okay', '[]', 5, 1)";

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&[
        "repository",
        "user",
        "policy",
        "spaces",
        "conduit",
        "auth",
        "worker",
        "project",
        "herald",
    ])
    .await?;
    test_db.execute(&SEED.replace("TOKEN", TOKEN)).await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn call(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_diffusion::register(&mut registry);
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
        "diffusion.repository.edit",
        &http,
        Mode::Serve,
        NOW,
    )
    .await
    .response
}

async fn rows(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .expect("rows")
        .iter()
        .map(|row| phab_db::rows::text(row, "value").expect("value"))
        .collect()
}

fn edit(id: Value, transactions: Value) -> Value {
    Value::Object(
        [
            ("objectIdentifier".to_owned(), id),
            ("transactions".to_owned(), transactions),
        ]
        .into_iter()
        .collect(),
    )
}

fn error(message: &str) -> Response {
    Response::Envelope(Envelope::error(
        "ERR-CONDUIT-CORE",
        format!("Validation errors:\n  - {message}"),
    ))
}

#[tokio::test]
async fn edits_repositories() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let response = call(
        &context,
        edit(
            json!("PHID-REPO-one"),
            json!([
                {"type": "name", "value": "One!"},
                {"type": "callsign", "value": "UNO"},
                {"type": "shortName", "value": "one"},
                {"type": "description", "value": "The first one."},
                {"type": "status", "value": "inactive"},
                {"type": "publish", "value": false},
            ]),
        ),
    )
    .await;
    let Response::Envelope(Envelope {
        result,
        error_code: None,
        ..
    }) = response
    else {
        panic!("expected an answer, got {response:?}");
    };
    assert_eq!(result["object"], json!({"id": 1, "phid": "PHID-REPO-one"}));
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(6));
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', name, callsign, repositorySlug, details, dateModified) \
               AS value FROM `{ns}_repository`.repository WHERE id = 1",
        )
        .await,
        [format!(
            "One! | UNO | one | {{\"tracking-enabled\":\"inactive\",\"default-branch\":\"main\",\
             \"remote-uri\":\"a\\/b\",\"description\":\"The first one.\",\
             \"newly-initialized\":false,\"herald-disabled\":1}} | {NOW}"
        )]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', transactionType, oldValue, newValue) AS value \
             FROM `{ns}_repository`.repository_transaction ORDER BY id",
        )
        .await,
        [
            "repo:name | \"One\" | \"One!\"",
            "repo:callsign | \"ONE\" | \"UNO\"",
            "repo:slug | null | \"one\"",
            "repo:description | null | \"The first one.\"",
            "repo:activate | true | \"inactive\"",
            "repo:notify | 1 | 0",
        ]
    );
    // The needs-update message is cleared.
    assert_eq!(
        rows(
            &test_db,
            "SELECT statusType AS value FROM `{ns}_repository`.repository_statusmessage",
        )
        .await,
        ["fetch"]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT objectPHID AS value FROM `{ns}_worker`.worker_activetask",
        )
        .await,
        ["PHID-REPO-one"]
    );

    // Removing the callsign and the short name; the status always has an
    // effect.
    let response = call(
        &context,
        edit(
            json!(1),
            json!([
                {"type": "callsign", "value": ""},
                {"type": "shortName", "value": ""},
                {"type": "status", "value": "inactive"},
            ]),
        ),
    )
    .await;
    assert!(
        matches!(
            response,
            Response::Envelope(Envelope {
                error_code: None,
                ..
            })
        ),
        "{response:?}"
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', IFNULL(callsign, 'NULL'), IFNULL(repositorySlug, 'NULL')) \
               AS value FROM `{ns}_repository`.repository WHERE id = 1",
        )
        .await,
        ["NULL | NULL"]
    );

    // Validation errors.
    for (transaction, message) in [
        (
            json!({"type": "callsign", "value": "OTHER"}),
            "The selected callsign (\"OTHER\") is already in use by another repository. \
             Choose a unique callsign.",
        ),
        (
            json!({"type": "callsign", "value": "lower"}),
            "The callsign \"lower\" is not a valid repository callsign. Callsigns may only \
             contain UPPERCASE letters.",
        ),
        (
            json!({"type": "shortName", "value": "other"}),
            "The selected repository short name is already in use by another repository. \
             Choose a unique short name.",
        ),
        (
            json!({"type": "shortName", "value": "123"}),
            "The name \"123\" is not a valid repository short name. Repository short names may \
             not contain only numbers.",
        ),
        (
            json!({"type": "status", "value": "dormant"}),
            "Repository status \"dormant\" is not valid. Valid statuses are: active, inactive.",
        ),
    ] {
        assert_eq!(
            call(&context, edit(json!(1), json!([transaction.clone()]))).await,
            error(message),
            "{transaction}"
        );
    }
    assert_eq!(
        call(
            &context,
            edit(json!(1), json!([{"type": "publish", "value": "yes"}]))
        )
        .await,
        Response::Envelope(Envelope::error(
            "ERR-CONDUIT-CORE",
            "Exception when processing transaction of type \"publish\": Error while reading \
             \"value\": Expected boolean (true or false), got something else."
        ))
    );

    // Left to PHP.
    for params in [
        edit(json!(3), json!([{"type": "name", "value": "X"}])),
        edit(json!(1), json!([{"type": "name", "value": "One!"}])),
        edit(json!(1), json!([{"type": "name", "value": ""}])),
        edit(
            json!(1),
            json!([{"type": "description", "value": "See D12."}]),
        ),
        edit(
            json!(1),
            json!([{"type": "description", "value": "Ask @alice"}]),
        ),
        edit(json!(1), json!([{"type": "defaultBranch", "value": "dev"}])),
        json!({"transactions": [{"type": "name", "value": "New"}]}),
    ] {
        let response = call(&context, params.clone()).await;
        assert!(
            matches!(response, Response::Fallback(_)),
            "{params}: {response:?}"
        );
    }

    test_db.drop_databases().await;
}
