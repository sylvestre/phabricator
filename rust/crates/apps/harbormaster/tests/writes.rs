//! `harbormaster.sendmessage` and `harbormaster.createartifact` against a
//! test database.
//!
//! Alice, who calls, sees the buildables of the open revision's diff and
//! their builds. Plan 1 is runnable by those who see it, plan 2 only by
//! administrators, plan 3 an autoplan, plan 4 never restartable and plan
//! 5 restartable if failed.

// Few databases, many scenarios each: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::{Config, hash};
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
    "file",
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
    (2, 'PHID-DIFF-secret', NULL, 'PHID-USER-bob', 0, 0, 1, 1, 1, 'no-one'), \
    (3, 'PHID-DIFF-older', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'no-one'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildable (id, phid, buildablePHID, \
      containerPHID, buildableStatus, dateCreated, dateModified, isManualBuildable) VALUES \
    (1, 'PHID-HMBB-1', 'PHID-DIFF-open', 'PHID-DREV-open', 'building', 10, 11, 0), \
    (2, 'PHID-HMBB-2', 'PHID-DIFF-secret', NULL, 'building', 20, 21, 0), \
    (3, 'PHID-HMBB-3', 'PHID-DIFF-older', 'PHID-DREV-open', 'building', 30, 31, 0); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildplan (id, phid, name, planStatus, \
      dateCreated, dateModified, planAutoKey, viewPolicy, editPolicy, properties) VALUES \
    (1, 'PHID-HMCP-1', 'Try', 'active', 1, 1, NULL, 'users', 'admin', \
      '{\"behavior.runnable\":\"view\"}'), \
    (2, 'PHID-HMCP-2', 'Locked', 'active', 1, 1, NULL, 'users', 'admin', '[]'), \
    (3, 'PHID-HMCP-3', 'arcanist', 'active', 1, 1, 'arcanist', 'admin', 'admin', '{}'), \
    (4, 'PHID-HMCP-4', 'Once', 'active', 1, 1, NULL, 'users', 'users', \
      '{\"behavior.restartable\":\"never\"}'), \
    (5, 'PHID-HMCP-5', 'Retry', 'active', 1, 1, NULL, 'users', 'users', \
      '{\"behavior.restartable\":\"failed\"}'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_build (id, phid, buildablePHID, \
      buildPlanPHID, buildStatus, dateCreated, dateModified, buildGeneration, planAutoKey, \
      buildParameters, initiatorPHID) VALUES \
    (1, 'PHID-HMBD-1', 'PHID-HMBB-1', 'PHID-HMCP-1', 'failed', 1, 1, 2, NULL, '{}', NULL), \
    (2, 'PHID-HMBD-2', 'PHID-HMBB-1', 'PHID-HMCP-2', 'building', 1, 1, 0, NULL, '{}', NULL), \
    (3, 'PHID-HMBD-3', 'PHID-HMBB-1', 'PHID-HMCP-3', 'building', 1, 1, 0, 'arcanist', '{}', \
      'PHID-USER-alice'), \
    (4, 'PHID-HMBD-4', 'PHID-HMBB-3', 'PHID-HMCP-3', 'building', 1, 1, 0, 'arcanist', '{}', \
      'PHID-USER-bob'), \
    (5, 'PHID-HMBD-5', 'PHID-HMBB-1', 'PHID-HMCP-4', 'paused', 1, 1, 0, NULL, '{}', NULL), \
    (6, 'PHID-HMBD-6', 'PHID-HMBB-2', 'PHID-HMCP-1', 'building', 1, 1, 0, NULL, '{}', NULL); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildtarget (id, phid, buildPHID, \
      buildStepPHID, className, details, variables, dateCreated, dateModified, targetStatus, \
      name, dateStarted, dateCompleted, buildGeneration) VALUES \
    (1, 'PHID-HMBT-1', 'PHID-HMBD-1', 'PHID-HMCS-1', \
      'HarbormasterHTTPRequestBuildStepImplementation', '{}', '{}', 1, 1, 'target/waiting', \
      'Run', NULL, NULL, 2), \
    (2, 'PHID-HMBT-2', 'PHID-HMBD-2', 'PHID-HMCS-1', \
      'HarbormasterHTTPRequestBuildStepImplementation', '{}', '{}', 1, 1, 'target/waiting', \
      'Run', NULL, NULL, 0), \
    (3, 'PHID-HMBT-3', 'PHID-HMBD-3', 'PHID-HMCS-2', \
      'HarbormasterArcUnitBuildStepImplementation', '{}', '{}', 1, 1, 'target/waiting', \
      'arcanist.unit', NULL, NULL, 0), \
    (4, 'PHID-HMBT-4', 'PHID-HMBD-4', 'PHID-HMCS-2', \
      'HarbormasterArcUnitBuildStepImplementation', '{}', '{}', 1, 1, 'target/waiting', \
      'arcanist.unit', NULL, NULL, 0), \
    (5, 'PHID-HMBT-5', 'PHID-HMBD-6', 'PHID-HMCS-1', \
      'HarbormasterHTTPRequestBuildStepImplementation', '{}', '{}', 1, 1, 'target/waiting', \
      'Run', NULL, NULL, 0); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_string (stringIndex, stringValue) VALUES \
    ('STRINGINDEX', 'test_known')";

fn config() -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = json!({
        "phabricator.base-uri": "http://phabricator.test/",
    }) {
        config.push_layer("test", map);
    }
    config
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    let index = hash::digest_for_index(b"test_known");
    test_db
        .execute(&SEED.replace("TOKEN", TOKEN).replace("STRINGINDEX", &index))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: config(),
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

async fn messages(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', id, authorPHID, receiverPHID, type, isConsumed, dateCreated, \
           dateModified) AS line FROM `{ns}_harbormaster`.harbormaster_buildmessage ORDER BY id",
    )
    .await
}

async fn tasks(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', t.id, t.taskClass, t.priority, t.objectPHID, \
           IFNULL(t.containerPHID, '-'), IFNULL(t.leaseOwner, '-'), t.dateCreated, d.data) \
           AS line FROM `{ns}_worker`.worker_activetask t JOIN `{ns}_worker`.worker_taskdata d \
           ON d.id = t.dataID ORDER BY t.id",
    )
    .await
}

async fn send_to_targets(test_db: &TestDatabase, context: &Context) {
    let method = "harbormaster.sendmessage";
    let result = answer(
        context,
        method,
        json!({
            "receiver": "PHID-HMBT-1",
            "type": "fail",
            "lint": [
                {
                    "name": "Missing semicolon",
                    "code": "clang-format",
                    "severity": "error",
                    "path": "dom/base/Harbor.cpp",
                    "line": 4,
                    "char": 11,
                    "description": "Add a / here",
                    "ignored": true,
                },
                {
                    "name": "Function could be const",
                    "code": "readability",
                    "severity": "warning",
                    "path": "dom/base/Harbor.cpp",
                },
            ],
            "unit": [
                {
                    "name": "test_ships",
                    "namespace": "dom/base/test",
                    "engine": "mochitest",
                    "result": "fail",
                    "duration": 1.25,
                    "details": "Expected 3 ships, got 2.\nStack: \u{e9}",
                },
                {
                    "name": "test_known",
                    "result": "pass",
                    "path": "a.cpp",
                    "coverage": {"a.cpp": "NCU", "b": [1, {"0": "x"}]},
                    "format": "remarkup",
                    "details": "0",
                },
            ],
        }),
    )
    .await;
    assert_eq!(result, Value::Null);
    assert_eq!(
        lines(
            test_db,
            "SELECT CONCAT_WS(' ', id, buildTargetPHID, path, IFNULL(line, '-'), \
               IFNULL(characterOffset, '-'), code, severity, name, properties, dateCreated, \
               dateModified) AS line FROM `{ns}_harbormaster`.harbormaster_buildlintmessage \
               ORDER BY id",
        )
        .await,
        [
            "1 PHID-HMBT-1 dom/base/Harbor.cpp 4 11 clang-format error Missing semicolon \
             {\"description\":\"Add a \\/ here\"} 1700000000 1700000000",
            "2 PHID-HMBT-1 dom/base/Harbor.cpp - - readability warning Function could be const \
             [] 1700000000 1700000000",
        ]
    );
    let ships = hash::digest_for_index(b"test_ships");
    let known = hash::digest_for_index(b"test_known");
    assert_eq!(
        lines(
            test_db,
            "SELECT CONCAT_WS(' ', id, buildTargetPHID, engine, namespace, CONCAT('[', name, ']'), \
               nameIndex, result, duration, properties, dateCreated, dateModified) AS line \
               FROM `{ns}_harbormaster`.harbormaster_buildunitmessage ORDER BY id",
        )
        .await,
        [
            format!(
                "1 PHID-HMBT-1 mochitest dom/base/test [] {ships} fail 1.25 \
                 {{\"details\":\"Expected 3 ships, got 2.\\nStack: \\u00e9\"}} 1700000000 \
                 1700000000"
            ),
            format!(
                "2 PHID-HMBT-1   [] {known} pass 0 {{\"path\":\"a.cpp\",\"coverage\":\
                 {{\"a.cpp\":\"NCU\",\"b\":[1,[\"x\"]]}},\"format\":\"remarkup\"}} 1700000000 \
                 1700000000"
            ),
        ]
    );
    assert_eq!(
        lines(
            test_db,
            "SELECT CONCAT_WS(' ', id, stringIndex, stringValue) AS line \
               FROM `{ns}_harbormaster`.harbormaster_string ORDER BY id",
        )
        .await,
        [
            format!("1 {known} test_known"),
            format!("2 {ships} test_ships"),
        ]
    );
    assert_eq!(
        messages(test_db).await,
        ["1 PHID-USER-alice PHID-HMBT-1 fail 0 1700000000 1700000000"]
    );
    assert_eq!(
        tasks(test_db).await,
        ["1 HarbormasterBuildWorker 2000 PHID-HMBD-1 - - 1700000000 {\"buildID\":\"1\"}",]
    );

    // The deprecated parameter, any message type, an autoplan build the
    // caller initiated.
    answer(
        context,
        method,
        json!({"buildTargetPHID": "PHID-HMBT-3", "type": "work", "lint": null}),
    )
    .await;
    assert_eq!(
        messages(test_db).await[1],
        "2 PHID-USER-alice PHID-HMBT-3 work 0 1700000000 1700000000"
    );
    assert_eq!(
        tasks(test_db).await[1],
        "2 HarbormasterBuildWorker 2000 PHID-HMBD-3 - - 1700000000 {\"buildID\":\"3\"}"
    );

    assert_eq!(
        error_info(
            context,
            method,
            json!({"receiver": "PHID-HMBT-2", "type": "pass"})
        )
        .await,
        "[You Shall Not Pass: Plan 2 Locked] (Can Edit) You do not have permission to edit this \
         object. // Administrators can take this action. You must have edit permission on this \
         build plan to pause, abort, resume, or restart it. You must have edit permission on \
         this build plan to run it manually."
    );
    assert_eq!(
        error_info(
            context,
            method,
            json!({"receiver": "PHID-HMBT-4", "type": "pass"})
        )
        .await,
        "You can not send a message to this build target. Only the user who initiated the \
         build may report results for autoplan build targets."
    );
    // The target of a build of a buildable Alice can't see.
    assert_eq!(
        error_info(
            context,
            method,
            json!({"receiver": "PHID-HMBT-5", "type": "pass"})
        )
        .await,
        "Unable to load object \"PHID-HMBT-5\" to receive message."
    );
    for (params, expected) in [
        (
            json!({"type": "pass"}),
            "Call omits required \"receiver\" parameter. Specify the PHID of the object you want \
             to send a message to.",
        ),
        (
            json!({"receiver": "PHID-HMBT-1", "buildTargetPHID": "PHID-HMBT-1", "type": "pass"}),
            "Call specifies both \"receiver\" and \"buildTargetPHID\". When using the modern \
             \"receiver\" parameter, omit the deprecated \"buildTargetPHID\" parameter.",
        ),
        (
            json!({"receiver": "PHID-HMBT-1", "type": ""}),
            "Call omits required \"type\" parameter. Specify the type of message you want to \
             send.",
        ),
        (
            json!({"receiver": "PHID-HMBT-nope", "type": "pass"}),
            "Unable to load object \"PHID-HMBT-nope\" to receive message.",
        ),
        (
            json!({"receiver": "PHID-HMBD-1", "type": "pass", "unit": []}),
            "Call includes \"unit\" parameter. This parameter must be omitted when the receiver \
             is not a Build Target.",
        ),
    ] {
        assert_eq!(error_info(context, method, params).await, expected);
    }
    for params in [
        json!({"receiver": "B1", "type": "restart"}),
        json!({"receiver": "PHID-DREV-open", "type": "restart"}),
        json!({"receiver": "PHID-HMBT-1", "type": 1}),
        json!({"receiver": "PHID-HMBT-1", "type": "pass", "unit": [{"name": "n"}]}),
        json!({"receiver": "PHID-HMBT-1", "type": "pass", "lint": [{"name": "n", "code": "c",
            "severity": "s", "path": "p", "line": "4"}]}),
        json!({"receiver": "PHID-HMBT-1", "type": "much-too-long-type"}),
    ] {
        assert!(
            is_fallback(context, method, params.clone()).await,
            "{params}"
        );
    }
    assert_eq!(messages(test_db).await.len(), 2);
}

async fn send_to_builds(test_db: &TestDatabase, context: &Context) {
    let method = "harbormaster.sendmessage";
    for (params, expected) in [
        (
            json!({"receiver": "PHID-HMBD-1", "type": "abort"}),
            "Unable to Abort Build: You can not abort this biuld because it is already complete.",
        ),
        (
            json!({"receiver": "PHID-HMBD-1", "type": "pause"}),
            "Unable to Pause Build: You can not pause this build because it has already \
             completed.",
        ),
        (
            json!({"receiver": "PHID-HMBD-1", "type": "resume"}),
            "Unable to Resume Build: You can not resume this build because it is not paused. \
             You can only resume a paused build.",
        ),
        (
            json!({"receiver": "PHID-HMBD-3", "type": "restart"}),
            "Can Not Restart Autobuild: This build can not be restarted because it is an \
             automatic build.",
        ),
        (
            json!({"receiver": "PHID-HMBD-5", "type": "restart"}),
            "Build Plan Prevents Restart: This build can not be restarted because the build plan \
             is configured to prevent the build from restarting.",
        ),
        (
            json!({"receiver": "PHID-HMBD-1", "type": "pass"}),
            "Message type \"pass\" is not supported.",
        ),
        (
            json!({"receiver": "PHID-HMBB-1", "type": "work"}),
            "Message type \"work\" is not supported.",
        ),
    ] {
        assert_eq!(error_info(context, method, params).await, expected);
    }
    let before = messages(test_db).await.len();

    assert_eq!(
        answer(
            context,
            method,
            json!({"receiver": "PHID-HMBD-1", "type": "restart"})
        )
        .await,
        Value::Null
    );
    // No edit capability on the plan is needed to send a command.
    answer(
        context,
        method,
        json!({"receiver": "PHID-HMBD-2", "type": "pause"}),
    )
    .await;
    // The pending pause makes the build resumable.
    answer(
        context,
        method,
        json!({"receiver": "PHID-HMBD-2", "type": "resume"}),
    )
    .await;
    assert_eq!(
        messages(test_db).await[before..],
        [
            "3 PHID-USER-alice PHID-HMBD-1 restart 0 1700000000 1700000000",
            "4 PHID-USER-alice PHID-HMBD-2 pause 0 1700000000 1700000000",
            "5 PHID-USER-alice PHID-HMBD-2 resume 0 1700000000 1700000000",
        ]
    );
    assert_eq!(
        tasks(test_db).await[2..],
        [
            "3 HarbormasterBuildWorker 2000 PHID-HMBD-1 PHID-HMBB-1 - 1700000000 \
             {\"buildID\":\"1\"}",
            "4 HarbormasterBuildWorker 2000 PHID-HMBD-2 PHID-HMBB-1 - 1700000000 \
             {\"buildID\":\"2\"}",
            "5 HarbormasterBuildWorker 2000 PHID-HMBD-2 PHID-HMBB-1 - 1700000000 \
             {\"buildID\":\"2\"}",
        ]
    );

    // A buildable: each build which accepts the command, newest first.
    answer(
        context,
        method,
        json!({"receiver": "PHID-HMBB-1", "type": "abort"}),
    )
    .await;
    assert_eq!(
        messages(test_db).await[before + 3..],
        [
            "6 PHID-USER-alice PHID-HMBD-5 abort 0 1700000000 1700000000",
            "7 PHID-USER-alice PHID-HMBD-2 abort 0 1700000000 1700000000",
        ]
    );
    assert_eq!(tasks(test_db).await.len(), 7);
    // Builds the viewer can't see, and buildables of hidden objects.
    assert_eq!(
        error_info(
            context,
            method,
            json!({"receiver": "PHID-HMBD-6", "type": "abort"})
        )
        .await,
        "Unable to load object \"PHID-HMBD-6\" to receive message."
    );
    assert_eq!(
        error_info(
            context,
            method,
            json!({"receiver": "PHID-HMBB-2", "type": "abort"})
        )
        .await,
        "Unable to load object \"PHID-HMBB-2\" to receive message."
    );
}

async fn artifacts(test_db: &TestDatabase, context: &Context) {
    let method = "harbormaster.createartifact";
    let result = answer(
        context,
        method,
        json!({
            "buildTargetPHID": "PHID-HMBT-1",
            "artifactKey": "treeherder",
            "artifactType": "uri",
            "artifactData": {
                "name": "Treeherder",
                "uri": "https://treeherder.mozilla.org/jobs?repo=try",
                "ui.external": true,
            },
        }),
    )
    .await;
    let phid = result["data"][0]["phid"]
        .as_str()
        .expect("a PHID")
        .to_owned();
    assert!(phid.starts_with("PHID-HMBA-"), "{phid}");
    let index = hash::digest_for_index(b"PHID-HMBD-1\x002\x00treeherder");
    assert_eq!(
        lines(
            test_db,
            "SELECT CONCAT_WS(' ', id, phid, buildTargetPHID, artifactType, artifactIndex, \
               artifactKey, artifactData, isReleased, dateCreated, dateModified) AS line \
               FROM `{ns}_harbormaster`.harbormaster_buildartifact ORDER BY id",
        )
        .await,
        [format!(
            "1 {phid} PHID-HMBT-1 uri {index} treeherder {{\"name\":\"Treeherder\",\"uri\":\
             \"https:\\/\\/treeherder.mozilla.org\\/jobs?repo=try\",\"ui.external\":true}} 0 \
             1700000000 1700000000"
        )]
    );

    let uri = |uri: &str| {
        json!({
            "buildTargetPHID": "PHID-HMBT-1",
            "artifactKey": "other",
            "artifactType": "uri",
            "artifactData": {"uri": uri},
        })
    };
    for (params, expected) in [
        (
            uri("treeherder"),
            "Unable to identify the protocol for URI \"treeherder\". URIs must be fully \
             qualified and have an identifiable protocol.",
        ),
        (
            uri("ftp://example.com/"),
            "URI \"ftp://example.com/\" does not have an allowable protocol. Configure \
             protocols in `uri.allowed-protocols`. Allowed protocols are: http, https, mailto.",
        ),
        (
            json!({"buildTargetPHID": "PHID-HMBT-nope", "artifactKey": "k",
                "artifactType": "uri", "artifactData": {"uri": "https://x"}}),
            "No such build target \"PHID-HMBT-nope\"!",
        ),
        (
            json!({"buildTargetPHID": "PHID-HMBT-1", "artifactKey": "k",
                "artifactType": "host", "artifactData": {}}),
            "There is no implementation available for artifacts of type \"host\".",
        ),
        (
            json!({"buildTargetPHID": "PHID-HMBT-1", "artifactKey": "k",
                "artifactType": "file", "artifactData": {"filePHID": "PHID-FILE-nope"}}),
            "File PHID \"PHID-FILE-nope\" does not correspond to a valid file.",
        ),
        (
            json!({"buildTargetPHID": "PHID-HMBT-2", "artifactKey": "k",
                "artifactType": "uri", "artifactData": {"uri": "https://x"}}),
            "[You Shall Not Pass: Plan 2 Locked] (Can Edit) You do not have permission to edit \
             this object. // Administrators can take this action. You must have edit permission \
             on this build plan to pause, abort, resume, or restart it. You must have edit \
             permission on this build plan to run it manually.",
        ),
        (
            json!({"buildTargetPHID": "PHID-HMBT-4", "artifactKey": "k",
                "artifactType": "uri", "artifactData": {"uri": "https://x"}}),
            "You can not create an artifact for this build target. Only the user who initiated \
             the build may create artifacts for autoplan build targets.",
        ),
    ] {
        assert_eq!(error_info(context, method, params).await, expected);
    }
    for params in [
        // The same key in the same generation.
        json!({"buildTargetPHID": "PHID-HMBT-1", "artifactKey": "treeherder",
            "artifactType": "uri", "artifactData": {"uri": "https://x"}}),
        json!({"buildTargetPHID": "PHID-HMBT-1", "artifactKey": "k", "artifactType": "uri",
            "artifactData": {"uri": "https://x", "ui.external": 1}}),
        json!({"buildTargetPHID": "PHID-HMBT-1", "artifactKey": "k", "artifactType": "uri",
            "artifactData": {"uri": "https://x", "extra": "y"}}),
    ] {
        assert!(
            is_fallback(context, method, params.clone()).await,
            "{params}"
        );
    }
    assert_eq!(
        lines(
            test_db,
            "SELECT CAST(COUNT(*) AS CHAR) AS line \
               FROM `{ns}_harbormaster`.harbormaster_buildartifact",
        )
        .await,
        ["1"]
    );
}

#[tokio::test]
async fn harbormaster_writes() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    send_to_targets(&test_db, &context).await;
    send_to_builds(&test_db, &context).await;
    artifacts(&test_db, &context).await;
    test_db.drop_databases().await;
}
