//! `merge.conflict.status` (`DifferentialMergeConflictStatusField`) in
//! `differential.revision.search`, `differential.query` and
//! `differential.getrevision` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Map, Value, json};

const TOKEN: &str = "api-mmmmmmmmmmmmmmmmmmmmmmmmmmmm";

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

const REDACTED: &str = "This revision lands on top of revisions you do not have permission to \
     see, so what the check ran against is not shown.";

/// Revisions `(id, status, repository, view policy)`; each has diff `id`
/// (`PHID-DIFF-<id>`) as its active diff.
const REVISIONS: &[(i64, &str, &str, &str)] = &[
    (1, "needs-review", "'PHID-REPO-git'", "users"),
    (2, "needs-review", "'PHID-REPO-git'", "users"),
    (3, "needs-review", "'PHID-REPO-git'", "users"),
    (4, "needs-review", "'PHID-REPO-git'", "users"),
    (5, "needs-review", "'PHID-REPO-git'", "users"),
    (6, "needs-review", "'PHID-REPO-git'", "users"),
    (7, "needs-review", "'PHID-REPO-git'", "users"),
    (8, "needs-review", "'PHID-REPO-git'", "users"),
    (9, "needs-review", "'PHID-REPO-git'", "users"),
    (10, "needs-review", "'PHID-REPO-git'", "users"),
    // Bob can't see it.
    (11, "needs-review", "'PHID-REPO-git'", "PHID-USER-alice"),
    (12, "needs-review", "'PHID-REPO-git'", "users"),
    (13, "published", "'PHID-REPO-git'", "users"),
    (14, "needs-review", "'PHID-REPO-other'", "users"),
    (15, "needs-review", "'PHID-REPO-hg'", "users"),
    (16, "needs-review", "'PHID-REPO-git'", "users"),
    (17, "needs-review", "NULL", "users"),
    (18, "needs-review", "'PHID-REPO-git'", "users"),
];

/// Stored values `(revision, fieldValue)`.
const STORED: &[(i64, &str)] = &[
    (
        2,
        r#"{"status":"clean","reason":"Merges.","reasonCode":null,
           "baseDiffCreationMethod":"moz-phab-git","baseDiffSourceControlSystem":"git",
           "checkedAgainstCommit":"aaa","checkedAgainstBaseCommit":"bbb",
           "checkedAgainstBaseRevisionPHID":null,"checkedAgainstDiffPHID":"PHID-DIFF-2",
           "checkedAgainstDiffID":2,"checkedAgainstStackDiffPHIDs":["PHID-DIFF-2"],
           "epoch":1700000000}"#,
    ),
    (
        3,
        r#"{"status":"conflict","reason":"D3 conflicts.","reasonCode":null,
           "checkedAgainstBaseRevisionPHID":"PHID-DREV-1","checkedAgainstDiffPHID":"PHID-DIFF-3"}"#,
    ),
    (
        4,
        r#"{"status":"unknown","reason":"No base.","reasonCode":"base-missing",
           "baseDiffCreationMethod":"moz-phab-hg","baseDiffSourceControlSystem":"git",
           "checkedAgainstDiffPHID":"PHID-DIFF-4"}"#,
    ),
    // Computed for an older diff.
    (
        5,
        r#"{"status":"unknown","reasonCode":"internal-error","checkedAgainstDiffPHID":"PHID-DIFF-100"}"#,
    ),
    // On top of D7, still current.
    (
        6,
        r#"{"status":"clean","reasonCode":"patch-too-large","checkedAgainstDiffPHID":"PHID-DIFF-6",
           "checkedAgainstStackDiffPHIDs":["PHID-DIFF-7","PHID-DIFF-6"]}"#,
    ),
    // On top of an older diff of D9.
    (
        8,
        r#"{"status":"conflict","checkedAgainstDiffPHID":"PHID-DIFF-8",
           "checkedAgainstStackDiffPHIDs":["PHID-DIFF-101","PHID-DIFF-8"]}"#,
    ),
    // On top of D11, which Bob can't see.
    (
        10,
        r#"{"status":"conflict","reason":"D11 conflicts.","reasonCode":"parent-abandoned",
           "checkedAgainstDiffPHID":"PHID-DIFF-10",
           "checkedAgainstStackDiffPHIDs":["PHID-DIFF-11","PHID-DIFF-10"]}"#,
    ),
    // Merged from D11's landing.
    (
        12,
        r#"{"status":"clean","reasonCode":"base-not-ancestor",
           "checkedAgainstBaseRevisionPHID":"PHID-DREV-11","checkedAgainstDiffPHID":"PHID-DIFF-12"}"#,
    ),
    (13, r#"{"status":"clean"}"#),
    (14, r#"{"status":"clean"}"#),
    (15, r#"{"status":"clean"}"#),
    (16, "not json"),
    (17, r#"{"status":"clean"}"#),
    (18, r#"["x"]"#),
];

fn config(enabled: bool) -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = json!({
        "phabricator.base-uri": "http://phabricator.test/",
        "merge.conflict.enabled": enabled,
        "merge.conflict.repository-phids": ["PHID-REPO-git", "PHID-REPO-hg"],
    }) {
        config.push_layer("test", map);
    }
    config
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
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-git', 'Git', 'GIT', 'git', '{{}}', 1, 1, 'users', 'admin', 'admin'), \
             (2, 'PHID-REPO-other', 'Other', 'OTHER', 'git', '{{}}', 1, 1, 'users', 'admin', \
               'admin'), \
             (3, 'PHID-REPO-hg', 'Hg', 'HG', 'hg', '{{}}', 1, 1, 'users', 'admin', 'admin'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy) VALUES \
             (100, 'PHID-DIFF-100', 5, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'), \
             (101, 'PHID-DIFF-101', 9, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users')"
        ))
        .await;
    for (id, status, repository, policy) in REVISIONS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, \
                   status, summary, testPlan, authorPHID, dateCreated, dateModified, attached, \
                   mailKey, viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) \
                 VALUES ({id}, 'R{id}', 'PHID-DREV-{id}', '{status}', '', '', \
                   'PHID-USER-alice', {id}, {id}, '{{}}', 'k', '{policy}', '{policy}', \
                   {repository}, '{{}}', 'PHID-DIFF-{id}'); \
                 INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
                   authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
                   viewPolicy) VALUES \
                 ({id}, 'PHID-DIFF-{id}', {id}, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users')"
            ))
            .await;
    }
    let index = phab_core::hash::digest_for_index(b"differential:merge-conflict-status");
    for (revision, value) in STORED {
        let value = value.replace('\'', "''");
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_customfieldstorage (objectPHID, \
                   fieldIndex, fieldValue) VALUES ('PHID-DREV-{revision}', '{index}', '{value}')"
            ))
            .await;
    }
    let context = Context {
        database: test_db.database().clone(),
        config: config(true),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn call(context: &Context, method: &str, params: Value) -> Value {
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
    match call::dispatch(
        context,
        &registry,
        method,
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
    {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

/// The custom fields of revision `id` from each method, which must agree;
/// the keys of `differential.revision.search` are checked too.
async fn custom_fields(context: &Context, id: i64) -> Map<String, Value> {
    let search = call(
        context,
        "differential.revision.search",
        json!({"constraints": {"ids": [id]}}),
    )
    .await;
    let fields = search["data"][0]["fields"]
        .as_object()
        .expect("fields")
        .clone();
    let custom: Map<String, Value> = fields
        .into_iter()
        .skip_while(|(key, _)| key != "bugzilla.bug-id")
        .collect();
    let query = call(context, "differential.query", json!({"ids": [id]})).await;
    let mut auxiliary = query[0]["auxiliary"]
        .as_object()
        .expect("auxiliary")
        .clone();
    for key in ["phabricator:projects", "phabricator:depends-on"] {
        assert!(auxiliary.shift_remove(key).is_some(), "D{id}: {key}");
    }
    assert_eq!(
        Value::Object(auxiliary.clone()),
        Value::Object(custom.clone()),
        "D{id}"
    );
    assert_eq!(
        auxiliary.keys().collect::<Vec<_>>(),
        custom.keys().collect::<Vec<_>>()
    );
    let revision = call(
        context,
        "differential.getrevision",
        json!({"revision_id": id}),
    )
    .await;
    let mut auxiliary = revision["auxiliary"]
        .as_object()
        .expect("auxiliary")
        .clone();
    for key in ["phabricator:projects", "phabricator:depends-on"] {
        auxiliary.shift_remove(key);
    }
    assert_eq!(
        auxiliary.keys().collect::<Vec<_>>(),
        custom.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        Value::Object(auxiliary),
        Value::Object(custom.clone()),
        "D{id}"
    );
    custom
}

/// `merge.conflict.status` of revision `id`.
async fn status(context: &Context, id: i64) -> Value {
    let custom = custom_fields(context, id).await;
    // Between the other two fields, in the order of the class map.
    assert_eq!(
        custom.keys().collect::<Vec<_>>(),
        ["bugzilla.bug-id", "merge.conflict.status", "uplift.request"],
        "D{id}"
    );
    custom["merge.conflict.status"].clone()
}

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect()
}

#[tokio::test]
async fn shows_the_stored_status() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // Nothing stored, or nothing PHP decodes to an array: an empty array.
    for id in [1, 7, 16] {
        assert_eq!(
            status(&context, id).await,
            json!({"isStale": false, "hint": null}),
            "D{id}"
        );
    }
    // A list's keys are its indexes.
    assert_eq!(
        status(&context, 18).await,
        json!({"0": "x", "isStale": false, "hint": null})
    );
    // Stored keys first, in their order, without the stack.
    let clean = status(&context, 2).await;
    assert_eq!(
        keys(&clean),
        [
            "status",
            "reason",
            "reasonCode",
            "baseDiffCreationMethod",
            "baseDiffSourceControlSystem",
            "checkedAgainstCommit",
            "checkedAgainstBaseCommit",
            "checkedAgainstBaseRevisionPHID",
            "checkedAgainstDiffPHID",
            "checkedAgainstDiffID",
            "epoch",
            "isStale",
            "hint",
        ]
    );
    assert_eq!(clean["status"], "clean");
    assert_eq!(clean["checkedAgainstDiffID"], 2);
    assert_eq!(clean["epoch"], 1_700_000_000);
    assert_eq!(clean["isStale"], false);
    assert_eq!(clean["hint"], Value::Null);
    // The base revision is visible: nothing is redacted.
    assert_eq!(
        status(&context, 3).await,
        json!({
            "status": "conflict",
            "reason": "D3 conflicts.",
            "reasonCode": null,
            "checkedAgainstBaseRevisionPHID": "PHID-DREV-1",
            "checkedAgainstDiffPHID": "PHID-DIFF-3",
            "isStale": false,
            "hint": null,
        })
    );
    assert_eq!(
        status(&context, 4).await["hint"],
        "This revision was submitted with \"moz-phab-hg\", which records a Mercurial base \
         commit that does not exist in this Git repository. Submitting from a native Git \
         checkout lets the check run."
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn tells_stale_statuses() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // Computed for an older diff: no hint.
    assert_eq!(
        status(&context, 5).await,
        json!({
            "status": "unknown",
            "reasonCode": "internal-error",
            "checkedAgainstDiffPHID": "PHID-DIFF-100",
            "isStale": true,
            "hint": null,
        })
    );
    // The diff below is still current.
    assert_eq!(
        status(&context, 6).await,
        json!({
            "status": "clean",
            "reasonCode": "patch-too-large",
            "checkedAgainstDiffPHID": "PHID-DIFF-6",
            "isStale": false,
            "hint": "Splitting the change into smaller revisions brings the stack under the size \
                     the check can handle.",
        })
    );
    // The revision below has a newer diff.
    assert_eq!(status(&context, 8).await["isStale"], true);
    // ... or its diff is gone.
    test_db
        .execute("DELETE FROM `{ns}_differential`.differential_diff WHERE id = 7")
        .await;
    assert_eq!(status(&context, 6).await["isStale"], true);

    test_db.drop_databases().await;
}

#[tokio::test]
async fn redacts_what_the_viewer_cannot_see() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // D11 is below: the redacted keys are set where they were stored, and
    // appended after isStale where they weren't.
    let stacked = status(&context, 10).await;
    assert_eq!(
        stacked,
        json!({
            "status": "conflict",
            "reason": REDACTED,
            "reasonCode": null,
            "checkedAgainstDiffPHID": "PHID-DIFF-10",
            "isStale": false,
            "baseDiffCreationMethod": null,
            "baseDiffSourceControlSystem": null,
            "hint": null,
        })
    );
    assert_eq!(
        keys(&stacked),
        [
            "status",
            "reason",
            "reasonCode",
            "checkedAgainstDiffPHID",
            "isStale",
            "baseDiffCreationMethod",
            "baseDiffSourceControlSystem",
            "hint",
        ]
    );
    // D11's landing was the base.
    let based = status(&context, 12).await;
    assert_eq!(based["reason"], REDACTED);
    assert_eq!(based["reasonCode"], Value::Null);
    assert_eq!(based["hint"], Value::Null);

    test_db.drop_databases().await;
}

#[tokio::test]
async fn is_null_or_absent_where_it_does_not_apply() {
    let Some((test_db, mut context)) = setup().await else {
        return;
    };

    // Closed; a repository not in the list; a Mercurial one; none.
    for id in [13, 14, 15, 17] {
        assert_eq!(status(&context, id).await, Value::Null, "D{id}");
    }
    // Closed by accepting, with differential.close-on-accept.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_revision SET status = 'accepted' \
             WHERE id = 2",
        )
        .await;
    assert_eq!(status(&context, 2).await["status"], "clean");
    if let Value::Object(map) = json!({"differential.close-on-accept": true}) {
        context.config.push_layer("close", map);
    }
    assert_eq!(status(&context, 2).await, Value::Null);

    // Disabled: the field doesn't exist.
    context.config = config(false);
    for id in [1, 2, 13] {
        assert_eq!(
            custom_fields(&context, id).await.keys().collect::<Vec<_>>(),
            ["bugzilla.bug-id", "uplift.request"],
            "D{id}"
        );
    }

    test_db.drop_databases().await;
}
