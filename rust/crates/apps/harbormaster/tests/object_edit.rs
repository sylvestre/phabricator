//! `harbormaster.build.edit`, `harbormaster.buildable.edit` and
//! `harbormaster.step.edit` against a test database.
//!
//! Alice, who calls, sees buildable 1 (of the open revision's diff) and
//! its builds, not buildable 2 (of a secret diff) nor its build 6. She
//! can edit the steps of plan 4 (step 1 makes HTTP requests, step 3 is of
//! an unknown implementation, step 5's details are not stored as Lisk
//! writes them, step 6 waits), see those of plan 1, which only
//! administrators edit, and not those of plan 3.

// Few databases, many scenarios each: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-hmhmhmhmhmhmhmhmhmhmhmhmhmhm";
const NOW: i64 = 1_700_000_000;

const DATABASES: &[&str] = &[
    "harbormaster",
    "differential",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "worker",
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
    INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
      summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
      viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
    (1, 'Open', 'PHID-DREV-open', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
      'users', 'users', NULL, '{}', ''); \
    INSERT INTO `{ns}_differential`.differential_diff (id, phid, revisionID, authorPHID, \
      lintStatus, unitStatus, lineCount, dateCreated, dateModified, viewPolicy) VALUES \
    (1, 'PHID-DIFF-open', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'no-one'), \
    (2, 'PHID-DIFF-secret', NULL, 'PHID-USER-bob', 0, 0, 1, 1, 1, 'no-one'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildable (id, phid, buildablePHID, \
      containerPHID, buildableStatus, dateCreated, dateModified, isManualBuildable) VALUES \
    (1, 'PHID-HMBB-1', 'PHID-DIFF-open', 'PHID-DREV-open', 'building', 10, 11, 0), \
    (2, 'PHID-HMBB-2', 'PHID-DIFF-secret', NULL, 'building', 20, 21, 0); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildplan (id, phid, name, planStatus, \
      dateCreated, dateModified, planAutoKey, viewPolicy, editPolicy, properties) VALUES \
    (1, 'PHID-HMCP-1', 'Try', 'active', 1, 1, NULL, 'users', 'admin', '[]'), \
    (3, 'PHID-HMCP-3', 'Hidden', 'active', 1, 1, NULL, 'admin', 'admin', '{}'), \
    (4, 'PHID-HMCP-4', 'Once', 'active', 1, 1, NULL, 'users', 'users', '[]'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_build (id, phid, buildablePHID, \
      buildPlanPHID, buildStatus, dateCreated, dateModified, buildGeneration, planAutoKey, \
      buildParameters, initiatorPHID) VALUES \
    (1, 'PHID-HMBD-1', 'PHID-HMBB-1', 'PHID-HMCP-1', 'failed', 1, 1, 2, NULL, '[]', NULL), \
    (2, 'PHID-HMBD-2', 'PHID-HMBB-1', 'PHID-HMCP-1', 'failed', 1, 1, 2, NULL, '{}', NULL), \
    (6, 'PHID-HMBD-6', 'PHID-HMBB-2', 'PHID-HMCP-1', 'building', 1, 1, 0, NULL, '[]', NULL); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildstep (id, phid, buildPlanPHID, \
      className, details, sequence, dateCreated, dateModified, name, description, \
      stepAutoKey) VALUES \
    (1, 'PHID-HMCS-1', 'PHID-HMCP-4', 'HarbormasterHTTPRequestBuildStepImplementation', \
      '{\"uri\":\"https:\\\\/\\\\/ci.test\\\\/run\",\"method\":\"POST\",\"dependsOn\":[]}', 0, \
      1, 1, 'Run', '', NULL), \
    (2, 'PHID-HMCS-2', 'PHID-HMCP-1', 'HarbormasterHTTPRequestBuildStepImplementation', \
      '[]', 0, 1, 1, 'Locked', '', NULL), \
    (3, 'PHID-HMCS-3', 'PHID-HMCP-4', 'HarbormasterCustomBuildStepImplementation', '[]', 0, \
      1, 1, 'Custom', '', NULL), \
    (4, 'PHID-HMCS-4', 'PHID-HMCP-3', 'HarbormasterArcUnitBuildStepImplementation', '[]', 0, \
      1, 1, NULL, '', 'arcanist.unit'), \
    (5, 'PHID-HMCS-5', 'PHID-HMCP-4', 'HarbormasterWaitForPreviousBuildStepImplementation', \
      '{\"x\": 1}', 0, 1, 1, 'Wait', '', NULL), \
    (6, 'PHID-HMCS-6', 'PHID-HMCP-4', 'HarbormasterWaitForPreviousBuildStepImplementation', \
      '[]', 0, 1, 1, 'Wait', '', NULL)";

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

async fn call(context: &Context, method: &str, params: Value) -> Response {
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
    call::dispatch(context, &registry, method, &http, Mode::Serve, NOW)
        .await
        .response
}

async fn answer(context: &Context, method: &str, params: Value) -> Value {
    match call(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

async fn error_info(context: &Context, method: &str, params: Value) -> String {
    match call(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info: Some(info),
            ..
        }) if code == "ERR-CONDUIT-CORE" => info,
        other => panic!("{method} {params}: expected an error, got {other:?}"),
    }
}

async fn is_fallback(context: &Context, method: &str, params: Value) -> bool {
    matches!(
        call(context, method, params).await,
        Response::Fallback { .. }
    )
}

/// The rows `sql` selects, as one `line` column each.
async fn lines(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| phab_db::rows::text(row, "line").expect("a line"))
        .collect()
}

/// The publish tasks, as their object and transaction PHIDs.
async fn tasks(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' | ', t.taskClass, t.objectPHID, \
           SUBSTRING_INDEX(SUBSTRING_INDEX(d.data, '\"xactionPHIDs\":', -1), ',\"state\"', 1)) \
           AS line \
         FROM `{ns}_worker`.worker_activetask t \
         JOIN `{ns}_worker`.worker_taskdata d ON d.id = t.dataID ORDER BY t.id",
    )
    .await
}

async fn builds_and_buildables(test_db: &TestDatabase, context: &Context) {
    for (method, hidden) in [
        ("harbormaster.build.edit", 6),
        ("harbormaster.buildable.edit", 2),
    ] {
        assert_eq!(
            error_info(
                context,
                method,
                json!({"objectIdentifier": 1, "transactions": [{"type": "name", "value": "x"}]}),
            )
            .await,
            "Transaction with key \"0\" has invalid type \"name\". This type is not \
             recognized. Valid types are: mfa."
        );
        assert_eq!(
            error_info(
                context,
                method,
                json!({"objectIdentifier": "99", "transactions": []})
            )
            .await,
            "No object exists with ID \"99\"."
        );
        assert_eq!(
            error_info(
                context,
                method,
                json!({"objectIdentifier": "PHID-HMBD-nope", "transactions": []}),
            )
            .await,
            "No object exists with PHID \"PHID-HMBD-nope\"."
        );
        // Left to PHP: creations, MFA and objects alice can't see.
        for params in [
            json!({"transactions": []}),
            json!({"objectIdentifier": 1, "transactions": [{"type": "mfa", "value": true}]}),
            json!({"objectIdentifier": hidden, "transactions": []}),
        ] {
            assert!(
                is_fallback(context, method, params.clone()).await,
                "{method} {params}"
            );
        }
    }
    // Lisk would write `{}` back as `[]`.
    assert!(
        is_fallback(
            context,
            "harbormaster.build.edit",
            json!({"objectIdentifier": 2, "transactions": []}),
        )
        .await
    );

    let result = answer(
        context,
        "harbormaster.build.edit",
        json!({"objectIdentifier": "PHID-HMBD-1", "transactions": []}),
    )
    .await;
    assert_eq!(
        result,
        json!({"object": {"id": 1, "phid": "PHID-HMBD-1"}, "transactions": []})
    );
    let result = answer(
        context,
        "harbormaster.buildable.edit",
        json!({"objectIdentifier": 1, "transactions": {}}),
    )
    .await;
    assert_eq!(
        result,
        json!({"object": {"id": 1, "phid": "PHID-HMBB-1"}, "transactions": []})
    );
    assert_eq!(
        lines(
            test_db,
            "SELECT CONCAT_WS(' | ', id, buildParameters, dateModified) AS line \
             FROM `{ns}_harbormaster`.harbormaster_build WHERE id IN (1, 2) ORDER BY id",
        )
        .await,
        [format!("1 | [] | {NOW}"), "2 | {} | 1".to_owned()]
    );
    assert_eq!(
        lines(
            test_db,
            "SELECT CONCAT_WS(' | ', id, dateModified) AS line \
             FROM `{ns}_harbormaster`.harbormaster_buildable ORDER BY id",
        )
        .await,
        [format!("1 | {NOW}"), "2 | 21".to_owned()]
    );
    assert_eq!(
        tasks(test_db).await,
        [
            "PhabricatorApplicationTransactionPublishWorker | PHID-HMBD-1 | []",
            "PhabricatorApplicationTransactionPublishWorker | PHID-HMBB-1 | []",
        ]
    );
}

async fn steps(test_db: &TestDatabase, context: &Context) {
    let method = "harbormaster.step.edit";
    assert_eq!(
        error_info(
            context,
            method,
            json!({"objectIdentifier": 1, "transactions": [
                {"type": "custom.uri", "value": "https://ci.test/b"},
                {"type": "name", "value": "x"},
            ]}),
        )
        .await,
        "Transaction with key \"1\" has invalid type \"name\". This type is not recognized. \
         Valid types are: custom.uri, custom.method, custom.credential, custom.content_type, \
         custom.http_body, custom.builtin.wait-for-message, mfa."
    );
    assert_eq!(
        error_info(
            context,
            method,
            json!({"objectIdentifier": 5, "transactions": [{"type": "custom.uri", "value": "x"}]}),
        )
        .await,
        "Transaction with key \"0\" has invalid type \"custom.uri\". This type is not \
         recognized. Valid types are: mfa."
    );
    assert_eq!(
        error_info(
            context,
            method,
            json!({"objectIdentifier": 1, "transactions": [{"type": "custom.uri", "value": 3}]}),
        )
        .await,
        "Exception when processing transaction of type \"custom.uri\": Error while reading \
         \"value\": Expected string, got something else."
    );
    // Left to PHP: credentials, MFA, steps alice can't edit or see,
    // unknown implementations, details Lisk would store differently.
    for params in [
        json!({"objectIdentifier": 1, "transactions": [
            {"type": "custom.credential", "value": "PHID-CDTL-1"},
        ]}),
        json!({"objectIdentifier": 1, "transactions": [{"type": "mfa", "value": true}]}),
        json!({"objectIdentifier": 2, "transactions": [{"type": "custom.uri", "value": "x"}]}),
        json!({"objectIdentifier": 4, "transactions": []}),
        json!({"objectIdentifier": 3, "transactions": []}),
        json!({"objectIdentifier": 5, "transactions": []}),
        json!({"transactions": []}),
    ] {
        assert!(
            is_fallback(context, method, params.clone()).await,
            "{params}"
        );
    }

    let result = answer(
        context,
        method,
        json!({"objectIdentifier": "PHID-HMCS-1", "transactions": [
            {"type": "custom.uri", "value": "https://ci.test/b"},
            {"type": "custom.builtin.wait-for-message", "value": "wait"},
            {"type": "custom.uri", "value": ""},
        ]}),
    )
    .await;
    assert_eq!(result["object"], json!({"id": 1, "phid": "PHID-HMCS-1"}));
    assert_eq!(result["transactions"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        lines(
            test_db,
            "SELECT CONCAT_WS(' | ', details, dateModified) AS line \
             FROM `{ns}_harbormaster`.harbormaster_buildstep WHERE id = 1",
        )
        .await,
        [format!(
            "{{\"uri\":\"\",\"method\":\"POST\",\"dependsOn\":[],\
             \"builtin.wait-for-message\":\"wait\"}} | {NOW}"
        )]
    );
    assert_eq!(
        lines(
            test_db,
            "SELECT CONCAT_WS(' | ', authorPHID, objectPHID, transactionType, oldValue, \
               newValue, contentSource, SUBSTRING_INDEX(metadata, ',\"core.groupID', 1), \
               dateCreated) AS line \
             FROM `{ns}_harbormaster`.harbormaster_buildsteptransaction ORDER BY id",
        )
        .await,
        [
            format!(
                "PHID-USER-alice | PHID-HMCS-1 | core:customfield | null | \
                 \"https:\\/\\/ci.test\\/b\" | {{\"source\":\"conduit\",\"params\":[]}} | \
                 {{\"customfield:key\":\"std:harbormaster:core:uri\" | {NOW}"
            ),
            format!(
                "PHID-USER-alice | PHID-HMCS-1 | core:customfield | null | \"wait\" | \
                 {{\"source\":\"conduit\",\"params\":[]}} | \
                 {{\"customfield:key\":\"std:harbormaster:core:builtin.wait-for-message\" | \
                 {NOW}"
            ),
            format!(
                "PHID-USER-alice | PHID-HMCS-1 | core:customfield | null | \"\" | \
                 {{\"source\":\"conduit\",\"params\":[]}} | \
                 {{\"customfield:key\":\"std:harbormaster:core:uri\" | {NOW}"
            ),
        ]
    );
    // A step without fields is saved.
    let result = answer(
        context,
        method,
        json!({"objectIdentifier": 6, "transactions": []}),
    )
    .await;
    assert_eq!(
        result,
        json!({"object": {"id": 6, "phid": "PHID-HMCS-6"}, "transactions": []})
    );
    assert_eq!(tasks(test_db).await.len(), 4);
}

#[tokio::test]
async fn harbormaster_edits() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    builds_and_buildables(&test_db, &context).await;
    steps(&test_db, &context).await;
    test_db.drop_databases().await;
}
