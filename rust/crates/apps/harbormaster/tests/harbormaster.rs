//! The Harbormaster methods against a test database.
//!
//! Bob sees the buildables of the diff of the open revision (1), of a diff
//! without revision (3), of his own private diff (4) and of a commit (6),
//! but not those of the hidden revision's diff (2) nor of Alice's private
//! diff (5); builds, targets, artifacts and logs follow, and he sees the
//! arcanist autoplan and the "Try" plan but not the "Secret" one.

// The checks share one database, so each method's are one long function.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-hhhhhhhhhhhhhhhhhhhhhhhhhhhh";

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
];

async fn query(context: &Context, method: &str, params: Value) -> Response {
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
        method,
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn answer(context: &Context, method: &str, params: Value) -> Value {
    match query(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

async fn error(context: &Context, method: &str, params: Value) -> (String, String) {
    match query(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info,
            ..
        }) => (code, error_info.unwrap_or_default()),
        other => panic!("{method} {params}: expected an error, got {other:?}"),
    }
}

async fn is_fallback(context: &Context, method: &str, params: Value) -> bool {
    matches!(
        query(context, method, params).await,
        Response::Fallback { .. }
    )
}

async fn ids(context: &Context, method: &str, params: Value) -> Vec<i64> {
    answer(context, method, params).await["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|row| row["id"].as_i64().expect("id"))
        .collect()
}

fn config() -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = json!({
        "phabricator.base-uri": "http://phabricator.test/",
        "phabricator.production-uri": "https://phabricator.example/",
    }) {
        config.push_layer("test", map);
    }
    config
}

const SEED: &str = "\
    INSERT INTO `{ns}_user`.user (phid, userName, realName, dateCreated, dateModified, \
      conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
      accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
    ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
    INSERT INTO `{ns}_conduit`.conduit_token \
      (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
    ('PHID-USER-bob', 'api', 'TOKEN', NULL, 1, 1); \
    INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, repositorySlug, \
      versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
      pushPolicy) VALUES \
    (1, 'PHID-REPO-vct', 'Version Control Tools', 'VCT', 'vct', 'git', '{}', 1, 1, 'users', \
      'admin', 'admin'); \
    INSERT INTO `{ns}_repository`.repository_commit (id, repositoryID, phid, \
      commitIdentifier, epoch, auditStatus, summary, importStatus, authorPHID) VALUES \
    (1, 1, 'PHID-CMIT-1', '1111111111111111111111111111111111111111', 5, 'none', 'S', 11, \
      'PHID-USER-alice'); \
    INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
      summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
      viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
    (1, 'Open', 'PHID-DREV-open', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
      'users', 'users', NULL, '{}', ''), \
    (3, 'Hidden', 'PHID-DREV-hidden', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', \
      'k', 'admin', 'admin', NULL, '{}', ''); \
    INSERT INTO `{ns}_differential`.differential_diff (id, phid, revisionID, authorPHID, \
      lintStatus, unitStatus, lineCount, dateCreated, dateModified, viewPolicy) VALUES \
    (1, 'PHID-DIFF-open', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'no-one'), \
    (2, 'PHID-DIFF-hidden', 3, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
    (3, 'PHID-DIFF-alone', NULL, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
    (4, 'PHID-DIFF-mine', NULL, 'PHID-USER-bob', 0, 0, 1, 1, 1, 'no-one'), \
    (5, 'PHID-DIFF-secret', NULL, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'no-one'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildable (id, phid, buildablePHID, \
      containerPHID, buildableStatus, dateCreated, dateModified, isManualBuildable) VALUES \
    (1, 'PHID-HMBB-1', 'PHID-DIFF-open', 'PHID-DREV-open', 'passed', 10, 11, 0), \
    (2, 'PHID-HMBB-2', 'PHID-DIFF-hidden', 'PHID-DREV-hidden', 'passed', 20, 21, 0), \
    (3, 'PHID-HMBB-3', 'PHID-DIFF-alone', NULL, 'building', 30, 31, 1), \
    (4, 'PHID-HMBB-4', 'PHID-DIFF-mine', NULL, 'failed', 40, 41, 0), \
    (5, 'PHID-HMBB-5', 'PHID-DIFF-secret', NULL, 'passed', 50, 51, 0), \
    (6, 'PHID-HMBB-6', 'PHID-CMIT-1', 'PHID-REPO-vct', 'odd', 60, 61, 0); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildplan (id, phid, name, planStatus, \
      dateCreated, dateModified, planAutoKey, viewPolicy, editPolicy, properties) VALUES \
    (1, 'PHID-HMCP-1', 'arcanist', 'active', 100, 101, 'arcanist', 'admin', 'admin', '{}'), \
    (2, 'PHID-HMCP-2', 'Try', 'active', 200, 201, NULL, 'users', 'admin', \
      '{\"behavior.hold-drafts\":\"never\",\"behavior.runnable\":\"view\"}'), \
    (3, 'PHID-HMCP-3', 'Secret', 'disabled', 300, 301, NULL, 'no-one', 'no-one', '[]'), \
    (4, 'PHID-HMCP-4', 'Another', 'disabled', 400, 401, NULL, 'users', 'users', '[]'); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildstep (id, phid, buildPlanPHID, \
      className, details, dateCreated, dateModified, sequence, name, description, \
      stepAutoKey) VALUES \
    (1, 'PHID-HMCS-1', 'PHID-HMCP-1', 'HarbormasterArcLintBuildStepImplementation', '{}', 1, \
      2, 0, 'arcanist.lint', '', 'arcanist.lint'), \
    (2, 'PHID-HMCS-2', 'PHID-HMCP-1', 'HarbormasterArcUnitBuildStepImplementation', '{}', 1, \
      2, 0, 'arcanist.unit', '', 'arcanist.unit'), \
    (3, 'PHID-HMCS-3', 'PHID-HMCP-2', 'HarbormasterHTTPRequestBuildStepImplementation', '{}', \
      3, 4, 0, NULL, 'Pings **it**.', NULL), \
    (4, 'PHID-HMCS-4', 'PHID-HMCP-3', 'HarbormasterHTTPRequestBuildStepImplementation', '{}', \
      3, 4, 0, 'Hidden', '', NULL); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_build (id, phid, buildablePHID, \
      buildPlanPHID, buildStatus, dateCreated, dateModified, buildGeneration, planAutoKey, \
      buildParameters, initiatorPHID) VALUES \
    (1, 'PHID-HMBD-1', 'PHID-HMBB-1', 'PHID-HMCP-1', 'passed', 1000, 1001, 1, 'arcanist', \
      '{}', 'PHID-USER-bob'), \
    (2, 'PHID-HMBD-2', 'PHID-HMBB-1', 'PHID-HMCP-3', 'building', 2000, 2001, 0, NULL, '{}', \
      NULL), \
    (3, 'PHID-HMBD-3', 'PHID-HMBB-2', 'PHID-HMCP-2', 'passed', 3000, 3001, 0, NULL, '{}', \
      NULL), \
    (4, 'PHID-HMBD-4', 'PHID-HMBB-6', 'PHID-HMCP-2', 'error', 4000, 4001, 0, NULL, '{}', ''); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildtarget (id, phid, buildPHID, \
      buildStepPHID, className, details, variables, dateCreated, dateModified, targetStatus, \
      name, dateStarted, dateCompleted, buildGeneration) VALUES \
    (1, 'PHID-HMBT-1', 'PHID-HMBD-1', 'PHID-HMCS-1', \
      'HarbormasterArcLintBuildStepImplementation', '{}', '{}', 40000001, 2, 'target/passed', \
      'arcanist.lint', 40000005, 40000009, 1), \
    (2, 'PHID-HMBT-2', 'PHID-HMBD-1', 'PHID-HMCS-2', \
      'HarbormasterArcUnitBuildStepImplementation', '{}', '{}', 40000002, 2, 'target/failed', \
      'arcanist.unit', NULL, NULL, 1), \
    (3, 'PHID-HMBT-3', 'PHID-HMBD-1', 'PHID-HMCS-1', \
      'HarbormasterArcLintBuildStepImplementation', '{}', '{}', 40000003, 2, 'target/passed', \
      'arcanist.lint', 0, 0, 0), \
    (4, 'PHID-HMBT-4', 'PHID-HMBD-3', 'PHID-HMCS-3', \
      'HarbormasterHTTPRequestBuildStepImplementation', '{}', '{}', 40000004, 2, \
      'target/pending', 'Hidden', NULL, NULL, 0), \
    (5, 'PHID-HMBT-5', 'PHID-HMBD-4', 'PHID-HMCS-3', \
      'HarbormasterHTTPRequestBuildStepImplementation', '{}', '{}', 40000005, 2, \
      'target/weird', 'Ping', 40000006, NULL, 0); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildartifact (id, phid, artifactType, \
      artifactIndex, artifactKey, artifactData, dateCreated, dateModified, buildTargetPHID, \
      isReleased) VALUES \
    (1, 'PHID-HMBA-1', 'uri', 'aaaaaaaaaaaa', 'results', '{}', 7, 8, 'PHID-HMBT-1', 1), \
    (2, 'PHID-HMBA-2', 'uri', 'bbbbbbbbbbbb', 'hidden', '{}', 7, 8, 'PHID-HMBT-4', 0); \
    INSERT INTO `{ns}_harbormaster`.harbormaster_buildlog (id, phid, logSource, logType, \
      duration, live, dateCreated, dateModified, buildTargetPHID, filePHID, byteLength, \
      chunkFormat, lineMap) VALUES \
    (1, 'PHID-HMCL-1', 's', 't', NULL, 0, 9, 10, 'PHID-HMBT-5', NULL, 1234, 'text', '[]'), \
    (2, 'PHID-HMCL-2', 's', 't', NULL, 0, 9, 10, 'PHID-HMBT-4', 'PHID-FILE-1', 5, 'text', \
      '[]'); \
    INSERT INTO `{ns}_harbormaster`.edge (src, type, dst, dateCreated, seq) VALUES \
    ('PHID-HMCP-2', 21, 'PHID-USER-alice', 1, 0), \
    ('PHID-HMCP-2', 21, 'PHID-USER-bob', 2, 0), \
    ('PHID-HMCP-2', 41, 'PHID-PROJ-ci', 1, 0)";

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db.execute(&SEED.replace("TOKEN", TOKEN)).await;
    let context = Context {
        database: test_db.database().clone(),
        config: config(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn buildables(context: &Context) {
    let method = "harbormaster.buildable.search";

    assert_eq!(ids(context, method, json!({})).await, [6, 4, 3, 1]);
    let found = answer(
        context,
        method,
        json!({"constraints": {"ids": [1, 6]}, "order": "oldest"}),
    )
    .await;
    assert_eq!(
        found,
        json!({
            "data": [
                {
                    "id": 1,
                    "type": "HMBB",
                    "phid": "PHID-HMBB-1",
                    "fields": {
                        "objectPHID": "PHID-DIFF-open",
                        "containerPHID": "PHID-DREV-open",
                        "buildableStatus": {"value": "passed"},
                        "isManual": false,
                        "uri": "http://phabricator.test/B1",
                        "dateCreated": 10,
                        "dateModified": 11,
                        "policy": {"view": "users", "edit": "users"},
                    },
                    "attachments": {},
                },
                {
                    "id": 6,
                    "type": "HMBB",
                    "phid": "PHID-HMBB-6",
                    "fields": {
                        "objectPHID": "PHID-CMIT-1",
                        "containerPHID": "PHID-REPO-vct",
                        "buildableStatus": {"value": "odd"},
                        "isManual": false,
                        "uri": "http://phabricator.test/B6",
                        "dateCreated": 60,
                        "dateModified": 61,
                        "policy": {"view": "users", "edit": "users"},
                    },
                    "attachments": {},
                },
            ],
            "maps": {},
            "query": {"queryKey": null},
            "cursor": {"limit": 100, "after": null, "before": null, "order": "oldest"},
        })
    );
    // A private diff's buildable has the diff's policy.
    assert_eq!(
        answer(context, method, json!({"constraints": {"ids": [4]}})).await["data"][0]["fields"]["policy"],
        json!({"view": "no-one", "edit": "no-one"})
    );

    // Constraints.
    let constrained =
        |constraints: Value| ids(context, method, json!({"constraints": constraints}));
    assert_eq!(constrained(json!({"manual": true})).await, [3]);
    assert_eq!(
        constrained(json!({"statuses": ["failed", "passed"]})).await,
        [4, 1]
    );
    assert_eq!(constrained(json!({"statuses": []})).await, [6, 4, 3, 1]);
    assert_eq!(
        constrained(json!({"objectPHIDs": ["PHID-DIFF-open", "PHID-DIFF-hidden"]})).await,
        [1]
    );
    // Only hidden objects: nothing matches.
    assert_eq!(
        constrained(json!({"objectPHIDs": ["PHID-DIFF-hidden"]})).await,
        Vec::<i64>::new()
    );
    assert_eq!(
        constrained(json!({"containerPHIDs": ["PHID-REPO-vct"]})).await,
        [6]
    );
    assert_eq!(constrained(json!({"phids": ["PHID-HMBB-3"]})).await, [3]);
    assert!(
        is_fallback(
            context,
            method,
            json!({"constraints": {"objectPHIDs": ["D1"]}})
        )
        .await
    );

    // Paging.
    let first = answer(context, method, json!({"limit": 2})).await;
    assert_eq!(
        first["cursor"],
        json!({"limit": 2, "after": "4", "before": null, "order": null})
    );
    let second = answer(context, method, json!({"limit": 2, "after": "4"})).await;
    assert_eq!(
        second["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].clone())
            .collect::<Vec<_>>(),
        [json!(3), json!(1)]
    );
    assert_eq!(
        second["cursor"],
        json!({"limit": 2, "after": null, "before": "3", "order": null})
    );
    assert_eq!(
        ids(context, method, json!({"limit": 1, "before": "3"})).await,
        [4]
    );
    // A cursor on a buildable Bob can't see.
    assert_eq!(
        error(context, method, json!({"after": "2"})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Cursor \"2\" does not identify a valid object in query \
             \"HarbormasterBuildableQuery\"."
                .to_owned()
        )
    );

    // Errors.
    assert_eq!(
        error(context, method, json!({"constraints": {"nope": 1}})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Constraint \"nope\" is not a valid constraint for this query.".to_owned()
        )
    );
    assert_eq!(
        error(context, method, json!({"order": "name"})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Query \"HarbormasterBuildableQuery\" does not support a builtin order \"name\". \
             Supported orders are: newest, created, oldest."
                .to_owned()
        )
    );
    assert_eq!(
        error(context, method, json!({"constraints": {"ids": []}})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Error while reading \"ids\": Expected a nonempty list, but value is an empty list."
                .to_owned()
        )
    );
    assert!(is_fallback(context, method, json!({"queryKey": "all"})).await);
}

async fn query_buildables(context: &Context) {
    let method = "harbormaster.querybuildables";
    assert_eq!(
        answer(
            context,
            method,
            json!({"buildablePHIDs": ["PHID-DIFF-open"]})
        )
        .await,
        json!({
            "data": [{
                "id": "1",
                "phid": "PHID-HMBB-1",
                "monogram": "B1",
                "uri": "https://phabricator.example/B1",
                "buildableStatus": "passed",
                "buildableStatusName": "Passed",
                "buildablePHID": "PHID-DIFF-open",
                "containerPHID": "PHID-DREV-open",
                "isManualBuildable": false,
            }],
            "cursor": {"limit": 100, "after": null, "before": null},
        })
    );
    let found = answer(
        context,
        method,
        json!({"manualBuildables": false, "limit": 1}),
    )
    .await;
    assert_eq!(
        found["data"][0]["buildableStatusName"],
        json!("Unknown (\"odd\")")
    );
    assert_eq!(
        found["cursor"],
        json!({"limit": 1, "after": "6", "before": null})
    );
    let found = answer(
        context,
        method,
        json!({"ids": [3, 5], "containerPHIDs": null}),
    )
    .await;
    assert_eq!(found["data"].as_array().unwrap().len(), 1);
    assert_eq!(found["data"][0]["containerPHID"], Value::Null);
    assert!(is_fallback(context, method, json!({"ids": []})).await);
    assert_eq!(
        answer(context, method, json!({"phids": ["PHID-HMBB-5"]})).await["data"],
        json!([])
    );
}

async fn builds(context: &Context) {
    let method = "harbormaster.build.search";
    assert_eq!(ids(context, method, json!({})).await, [4, 2, 1]);
    let found = answer(
        context,
        method,
        json!({"constraints": {"ids": [1, 2]}, "attachments": {"querybuilds": true}}),
    )
    .await;
    assert_eq!(
        found["data"],
        json!([
            {
                "id": 2,
                "type": "HMBD",
                "phid": "PHID-HMBD-2",
                "fields": {
                    "buildablePHID": "PHID-HMBB-1",
                    "buildPlanPHID": "PHID-HMCP-3",
                    "buildStatus": {"value": "building", "name": "Building", "color.ansi": "yellow"},
                    "initiatorPHID": null,
                    // Bob can't see the plan.
                    "name": "Build",
                    "dateCreated": 2000,
                    "dateModified": 2001,
                    "policy": {"view": "users", "edit": "users"},
                },
                "attachments": {
                    "querybuilds": {
                        "uri": "https://phabricator.example/harbormaster/build/2/",
                        "name": "Build",
                        "buildStatusName": "Building",
                    },
                },
            },
            {
                "id": 1,
                "type": "HMBD",
                "phid": "PHID-HMBD-1",
                "fields": {
                    "buildablePHID": "PHID-HMBB-1",
                    "buildPlanPHID": "PHID-HMCP-1",
                    "buildStatus": {"value": "passed", "name": "Passed", "color.ansi": "green"},
                    "initiatorPHID": "PHID-USER-bob",
                    "name": "arcanist",
                    "dateCreated": 1000,
                    "dateModified": 1001,
                    "policy": {"view": "users", "edit": "users"},
                },
                "attachments": {
                    "querybuilds": {
                        "uri": "https://phabricator.example/harbormaster/build/1/",
                        "name": "arcanist",
                        "buildStatusName": "Passed",
                    },
                },
            },
        ])
    );
    let found = answer(context, method, json!({"constraints": {"ids": [4]}})).await;
    assert_eq!(found["data"][0]["fields"]["initiatorPHID"], Value::Null);
    assert_eq!(
        found["data"][0]["fields"]["buildStatus"],
        json!({"value": "error", "name": "Unexpected Error", "color.ansi": "red"})
    );
    let constrained =
        |constraints: Value| ids(context, method, json!({"constraints": constraints}));
    assert_eq!(constrained(json!({"plans": ["PHID-HMCP-2"]})).await, [4]);
    assert_eq!(
        constrained(json!({"buildables": ["PHID-HMBB-1"]})).await,
        [2, 1]
    );
    assert_eq!(
        constrained(json!({"statuses": ["passed", "error"]})).await,
        [4, 1]
    );
    assert_eq!(
        constrained(json!({"initiators": ["PHID-USER-bob"]})).await,
        [1]
    );
    assert!(
        is_fallback(
            context,
            method,
            json!({"constraints": {"initiators": ["viewer()"]}})
        )
        .await
    );
    assert_eq!(
        error(context, method, json!({"constraints": {"plans": []}})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Error while reading \"plans\": Expected a nonempty list, but value is an empty list."
                .to_owned()
        )
    );
    assert_eq!(
        error(context, method, json!({"limit": 101})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Maximum page size for Conduit API method calls is 100, but this call specified 101."
                .to_owned()
        )
    );
}

async fn query_builds(context: &Context) {
    let method = "harbormaster.querybuilds";
    assert_eq!(
        answer(
            context,
            method,
            json!({"buildablePHIDs": ["PHID-HMBB-1"], "ids": [], "limit": 1})
        )
        .await,
        json!({
            "data": [{
                "id": 2,
                "type": "HMBD",
                "phid": "PHID-HMBD-2",
                "uri": "https://phabricator.example/harbormaster/build/2/",
                "name": "Build",
                "buildStatusName": "Building",
                "buildablePHID": "PHID-HMBB-1",
                "buildPlanPHID": "PHID-HMCP-3",
                "buildStatus": {"value": "building", "name": "Building"},
                "initiatorPHID": null,
                "dateCreated": 2000,
                "dateModified": 2001,
                "policy": {"view": "users", "edit": "users"},
            }],
            "maps": {},
            "query": {"queryKey": null},
            "cursor": {"limit": 1, "after": "2", "before": null, "order": null},
        })
    );
    assert_eq!(
        ids(context, method, json!({"buildStatuses": ["passed"]})).await,
        [1]
    );
    // The inner call is strictly typed.
    assert_eq!(
        error(context, method, json!({"ids": ["1"]})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Error while reading \"ids[0]\": Expected integer, got something else.".to_owned()
        )
    );
}

async fn plans_and_steps(context: &Context) {
    let method = "harbormaster.buildplan.search";
    assert_eq!(ids(context, method, json!({})).await, [4, 2, 1]);
    assert_eq!(
        ids(context, method, json!({"order": "name"})).await,
        [4, 1, 2]
    );
    assert_eq!(
        ids(context, method, json!({"order": "name", "after": "1"})).await,
        [2]
    );
    assert_eq!(
        ids(
            context,
            method,
            json!({"constraints": {"status": ["disabled"]}})
        )
        .await,
        [4]
    );
    assert_eq!(
        ids(context, method, json!({"constraints": {"match": ""}})).await,
        [4, 2, 1]
    );
    assert!(is_fallback(context, method, json!({"constraints": {"match": "Try"}})).await);
    assert!(
        is_fallback(
            context,
            method,
            json!({"constraints": {"projects": ["PHID-PROJ-ci"]}})
        )
        .await
    );
    // A cursor on a plan Bob can't see raises a policy exception in PHP.
    assert!(is_fallback(context, method, json!({"after": "3"})).await);
    let found = answer(
        context,
        method,
        json!({
            "constraints": {"ids": [1, 2]},
            "attachments": {"projects": true, "subscribers": true},
        }),
    )
    .await;
    assert_eq!(
        found["data"],
        json!([
            {
                "id": 2,
                "type": "HMCP",
                "phid": "PHID-HMCP-2",
                "fields": {
                    "name": "Try",
                    "status": {"value": "active"},
                    "behaviors": {
                        "hold-drafts": {"value": "never"},
                        "arc-land": {"value": "always"},
                        "buildable": {"value": "always"},
                        "restartable": {"value": "always"},
                        "runnable": {"value": "view"},
                    },
                    "dateCreated": 200,
                    "dateModified": 201,
                    "policy": {"view": "users", "edit": "admin"},
                },
                "attachments": {
                    "projects": {"projectPHIDs": ["PHID-PROJ-ci"]},
                    "subscribers": {
                        "subscriberPHIDs": ["PHID-USER-bob", "PHID-USER-alice"],
                        "subscriberCount": 2,
                        "viewerIsSubscribed": true,
                    },
                },
            },
            {
                "id": 1,
                "type": "HMCP",
                "phid": "PHID-HMCP-1",
                "fields": {
                    "name": "arcanist",
                    "status": {"value": "active"},
                    "behaviors": {
                        "hold-drafts": {"value": "always"},
                        "arc-land": {"value": "always"},
                        "buildable": {"value": "always"},
                        "restartable": {"value": "always"},
                        "runnable": {"value": "edit"},
                    },
                    "dateCreated": 100,
                    "dateModified": 101,
                    // An autoplan.
                    "policy": {"view": "users", "edit": "no-one"},
                },
                "attachments": {
                    "projects": {"projectPHIDs": []},
                    "subscribers": {
                        "subscriberPHIDs": [],
                        "subscriberCount": 0,
                        "viewerIsSubscribed": false,
                    },
                },
            },
        ])
    );

    let method = "harbormaster.step.search";
    assert_eq!(ids(context, method, json!({})).await, [3, 2, 1]);
    assert_eq!(
        answer(context, method, json!({"constraints": {"ids": [3]}})).await["data"],
        json!([{
            "id": 3,
            "type": "HMCS",
            "phid": "PHID-HMCS-3",
            "fields": {
                "name": "Make HTTP Request",
                "description": {"raw": "Pings **it**."},
                "buildPlanPHID": "PHID-HMCP-2",
                "dateCreated": 3,
                "dateModified": 4,
                "policy": {"view": "users", "edit": "admin"},
            },
            "attachments": {},
        }])
    );
    assert_eq!(
        error(context, method, json!({"constraints": {"plans": ["x"]}})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Constraint \"plans\" is not a valid constraint for this query.".to_owned()
        )
    );
}

async fn targets_artifacts_and_logs(context: &Context) {
    let method = "harbormaster.target.search";
    assert_eq!(ids(context, method, json!({})).await, [5, 3, 2, 1]);
    assert_eq!(
        answer(context, method, json!({"constraints": {"ids": [1, 5]}})).await["data"],
        json!([
            {
                "id": 5,
                "type": "HMBT",
                "phid": "PHID-HMBT-5",
                "fields": {
                    "name": "Ping",
                    "buildPHID": "PHID-HMBD-4",
                    "buildStepPHID": "PHID-HMCS-3",
                    "status": {"value": "target/weird", "name": "Unknown"},
                    "epochStarted": 40_000_006,
                    "epochCompleted": null,
                    "buildGeneration": 0,
                    "dateCreated": 40_000_005,
                    "dateModified": 2,
                    "policy": {"view": "users"},
                },
                "attachments": {},
            },
            {
                "id": 1,
                "type": "HMBT",
                "phid": "PHID-HMBT-1",
                "fields": {
                    "name": "Arcanist Lint Results",
                    "buildPHID": "PHID-HMBD-1",
                    "buildStepPHID": "PHID-HMCS-1",
                    "status": {"value": "target/passed", "name": "Passed"},
                    "epochStarted": 40_000_005,
                    "epochCompleted": 40_000_009,
                    "buildGeneration": 1,
                    "dateCreated": 40_000_001,
                    "dateModified": 2,
                    "policy": {"view": "users"},
                },
                "attachments": {},
            },
        ])
    );
    let constrained =
        |constraints: Value| ids(context, method, json!({"constraints": constraints}));
    assert_eq!(
        constrained(json!({"buildPHIDs": ["PHID-HMBD-1"]})).await,
        [3, 2, 1]
    );
    assert_eq!(
        constrained(json!({"createdStart": 40_000_002, "createdEnd": 40_000_003})).await,
        [3, 2]
    );
    assert_eq!(constrained(json!({"startedStart": 40_000_006})).await, [5]);
    assert_eq!(
        constrained(json!({"completedEnd": 40_000_009})).await,
        [3, 1]
    );
    assert_eq!(
        constrained(json!({"statuses": ["target/failed"]})).await,
        [2]
    );
    assert!(
        is_fallback(
            context,
            method,
            json!({"constraints": {"createdStart": 20_221_225}})
        )
        .await
    );
    assert_eq!(
        error(context, method, json!({"constraints": {"createdEnd": 0}})).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Error while reading \"createdEnd\": Epoch timestamp must be larger than 0, got 0."
                .to_owned()
        )
    );

    let method = "harbormaster.artifact.search";
    assert_eq!(
        answer(context, method, json!({})).await["data"],
        json!([{
            "id": 1,
            "type": "HMBA",
            "phid": "PHID-HMBA-1",
            "fields": {
                "buildTargetPHID": "PHID-HMBT-1",
                "artifactType": "uri",
                "artifactKey": "results",
                "isReleased": true,
                "dateCreated": 7,
                "dateModified": 8,
                "policy": {"view": "users"},
            },
            "attachments": {},
        }])
    );
    assert_eq!(
        ids(
            context,
            method,
            json!({"constraints": {"buildTargetPHIDs": ["PHID-HMBT-2"]}})
        )
        .await,
        Vec::<i64>::new()
    );
    assert!(
        is_fallback(
            context,
            method,
            json!({"constraints": {"phids": ["PHID-HMBA-1"]}})
        )
        .await
    );

    let method = "harbormaster.log.search";
    assert_eq!(
        answer(
            context,
            method,
            json!({"constraints": {"phids": ["PHID-HMCL-1", "PHID-HMCL-2"]}})
        )
        .await["data"],
        json!([{
            "id": 1,
            "type": "HMCL",
            "phid": "PHID-HMCL-1",
            "fields": {
                "buildTargetPHID": "PHID-HMBT-5",
                "byteLength": 1234,
                "filePHID": null,
                "dateCreated": 9,
                "dateModified": 10,
                "policy": {"view": "users"},
            },
            "attachments": {},
        }])
    );
}

async fn autotargets(context: &Context) {
    let method = "harbormaster.queryautotargets";
    assert_eq!(
        answer(
            context,
            method,
            json!({
                "objectPHID": "PHID-DIFF-open",
                "targetKeys": ["arcanist.unit", "arcanist.lint"],
            })
        )
        .await,
        json!({"targetMap": {"arcanist.unit": "PHID-HMBT-2", "arcanist.lint": "PHID-HMBT-1"}})
    );
    assert_eq!(
        answer(context, method, json!({"objectPHID": "PHID-DIFF-open"})).await,
        json!({"targetMap": []})
    );
    assert_eq!(
        error(
            context,
            method,
            json!({"objectPHID": "PHID-DIFF-open", "targetKeys": ["nope"]})
        )
        .await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "No build step provides autotarget \"nope\"!".to_owned()
        )
    );
    assert_eq!(
        error(
            context,
            method,
            json!({"objectPHID": "PHID-DIFF-gone", "targetKeys": ["arcanist.lint"]})
        )
        .await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "No such object \"PHID-DIFF-gone\" exists.".to_owned()
        )
    );
    // These would create a buildable, or a build.
    for object in ["PHID-DIFF-alone", "PHID-DIFF-mine"] {
        assert!(
            is_fallback(
                context,
                method,
                json!({"objectPHID": object, "targetKeys": ["arcanist.lint"]})
            )
            .await
        );
    }
    assert!(
        is_fallback(
            context,
            method,
            json!({"objectPHID": "D1", "targetKeys": ["arcanist.lint"]})
        )
        .await
    );
}

/// One database for every check, which only read it: creating the
/// schema is the slow part.
#[tokio::test]
async fn harbormaster_methods() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    buildables(&context).await;
    query_buildables(&context).await;
    builds(&context).await;
    query_builds(&context).await;
    plans_and_steps(&context).await;
    targets_artifacts_and_logs(&context).await;
    autotargets(&context).await;
    test_db.drop_databases().await;
}
