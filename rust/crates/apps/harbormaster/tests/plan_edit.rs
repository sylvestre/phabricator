//! `harbormaster.buildplan.edit` against a test database.
//!
//! Alice, who calls, can edit plan 1 and see plan 2, which only
//! administrators edit; plan 3 is an autoplan.

// Few databases, many scenarios each: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-pepepepepepepepepepepepepepe";
const NOW: i64 = 1_700_000_000;

const DATABASES: &[&str] = &[
    "harbormaster",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "worker",
    "project",
    "herald",
];

const SEED: &str = "\
    INSERT INTO `{ns}_user`.user (phid, userName, realName, dateCreated, dateModified, \
      conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
      accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
    ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
    INSERT INTO `{ns}_conduit`.conduit_token \
      (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
    ('PHID-USER-alice', 'api', 'TOKEN', NULL, 1, 1); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildplan (id, phid, name, planStatus, \
      dateCreated, dateModified, planAutoKey, viewPolicy, editPolicy, properties) VALUES \
    (1, 'PHID-HMCP-1', 'Try', 'active', 1, 1, NULL, 'users', 'users', \
      '{\"behavior.runnable\":\"view\",\"x\":\"a/b\"}'), \
    (2, 'PHID-HMCP-2', 'Locked', 'active', 1, 1, NULL, 'users', 'admin', '[]'), \
    (3, 'PHID-HMCP-3', 'arcanist', 'active', 1, 1, 'arcanist', 'users', 'users', '{}')";

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db.execute(&SEED.replace("TOKEN", TOKEN)).await;
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "http://phabricator.test/"}) {
        config.push_layer("test", map);
    }
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn call(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_harbormaster::register(&mut registry);
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
        "harbormaster.buildplan.edit",
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

#[tokio::test]
async fn edits_names_and_behaviors() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let response = call(
        &context,
        json!({
            "objectIdentifier": "PHID-HMCP-1",
            "transactions": [
                {"type": "name", "value": "Try harder"},
                {"type": "behavior.hold-drafts", "value": "never"},
                {"type": "behavior.runnable", "value": "view"},
            ],
        }),
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
    assert_eq!(result["object"], json!({"id": 1, "phid": "PHID-HMCP-1"}));
    // The behavior without effect is dropped.
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(2));

    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', name, properties, dateModified) AS value \
             FROM `{ns}_harbormaster`.harbormaster_buildplan WHERE id = 1",
        )
        .await,
        [format!(
            "Try harder | {{\"behavior.runnable\":\"view\",\"x\":\"a\\/b\",\
             \"behavior.hold-drafts\":\"never\"}} | {NOW}"
        )]
    );
    let transactions = rows(
        &test_db,
        "SELECT CONCAT_WS(' | ', authorPHID, objectPHID, viewPolicy, editPolicy, \
           transactionType, oldValue, newValue, contentSource, \
           CONCAT(SUBSTRING_INDEX(metadata, 'core.groupID', 1), 'core.groupID\":\"GROUP\"}'), \
           dateCreated) AS value \
         FROM `{ns}_harbormaster`.harbormaster_buildplantransaction ORDER BY id",
    )
    .await;
    assert_eq!(
        transactions,
        [
            format!(
                "PHID-USER-alice | PHID-HMCP-1 | public | PHID-USER-alice | harbormaster:name \
                 | \"Try\" | \"Try harder\" | {{\"source\":\"conduit\",\"params\":[]}} | \
                 {{\"core.groupID\":\"GROUP\"}} | {NOW}"
            ),
            format!(
                "PHID-USER-alice | PHID-HMCP-1 | public | PHID-USER-alice | behavior | \
                 \"always\" | \"never\" | {{\"source\":\"conduit\",\"params\":[]}} | \
                 {{\"behavior-key\":\"hold-drafts\",\"core.groupID\":\"GROUP\"}} | {NOW}"
            ),
        ]
    );
    let task = rows(
        &test_db,
        "SELECT CONCAT_WS(' | ', t.taskClass, t.objectPHID, t.priority, d.data) AS value \
         FROM `{ns}_worker`.worker_activetask t \
         JOIN `{ns}_worker`.worker_taskdata d ON d.id = t.dataID",
    )
    .await;
    assert_eq!(task.len(), 1);
    assert!(
        task[0].starts_with(
            "PhabricatorApplicationTransactionPublishWorker | PHID-HMCP-1 | 1000 | \
             {\"objectPHID\":\"PHID-HMCP-1\",\"actorPHID\":\"PHID-USER-alice\",\
             \"xactionPHIDs\":[\"PHID-XACT-HMCP-"
        ),
        "{task:?}"
    );
    assert!(
        task[0].ends_with(
            "\"state\":{\"parentMessageID\":null,\"isNewObject\":false,\
             \"heraldEmailPHIDs\":[],\"heraldForcedEmailPHIDs\":[],\"heraldHeader\":null,\
             \"mailToPHIDs\":[],\"mailCCPHIDs\":[],\"feedNotifyPHIDs\":[],\
             \"feedRelatedPHIDs\":[],\"feedShouldPublish\":false,\"mailShouldSend\":false,\
             \"mustEncrypt\":[],\"mailStamps\":[],\"mailUnexpandablePHIDs\":[],\
             \"mailMutedPHIDs\":[],\"webhookMap\":[],\"silent\":null,\"sendHistory\":false,\
             \"excludeMailRecipientPHIDs\":[],\"custom\":[],\"custom.encoding\":[]}}"
        ),
        "{task:?}"
    );

    // An invalid option.
    let response = call(
        &context,
        json!({
            "objectIdentifier": 1,
            "transactions": [{"type": "behavior.arc-land", "value": "sometimes"}],
        }),
    )
    .await;
    assert_eq!(
        response,
        Response::Envelope(Envelope::error(
            "ERR-CONDUIT-CORE",
            "Validation errors:\n  - Value \"sometimes\" is not a valid option for behavior \
             \"arc-land\". Valid options are: always, building, complete, never."
                .to_owned()
        ))
    );
    // A name which is not a string.
    let response = call(
        &context,
        json!({"objectIdentifier": 1, "transactions": [{"type": "name", "value": 5}]}),
    )
    .await;
    assert_eq!(
        response,
        Response::Envelope(Envelope::error(
            "ERR-CONDUIT-CORE",
            "Exception when processing transaction of type \"name\": Error while reading \
             \"value\": Expected string, got something else."
                .to_owned()
        ))
    );

    // Left to PHP: plans alice can't edit, autoplans, creations, other
    // types and edits without effect.
    for params in [
        json!({"objectIdentifier": 2, "transactions": [{"type": "name", "value": "X"}]}),
        json!({"objectIdentifier": 3, "transactions": [{"type": "name", "value": "X"}]}),
        json!({"transactions": [{"type": "name", "value": "X"}]}),
        json!({"objectIdentifier": 1, "transactions": [{"type": "view", "value": "users"}]}),
        json!({"objectIdentifier": 1, "transactions": [{"type": "name", "value": "Try harder"}]}),
        json!({"objectIdentifier": 1, "transactions": [{"type": "name", "value": ""}]}),
    ] {
        let response = call(&context, params.clone()).await;
        assert!(
            matches!(response, Response::Fallback(_)),
            "{params}: {response:?}"
        );
    }

    test_db.drop_databases().await;
}
