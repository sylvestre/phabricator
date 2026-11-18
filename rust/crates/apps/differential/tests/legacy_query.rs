//! `differential.query` and `differential.getrevision` against a test
//! database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-llllllllllllllllllllllllllll";

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
        "differential.query",
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

async fn ids(context: &Context, params: Value) -> Vec<String> {
    answer(context, params)
        .await
        .as_array()
        .expect("a list")
        .iter()
        .map(|revision| revision["id"].as_str().expect("id").to_owned())
        .collect()
}

/// Revisions `(id, author, status, dateModified, active diff)`.
const REVISIONS: &[(i64, &str, &str, i64, &str)] = &[
    (1, "bob", "needs-review", 30, "'PHID-DIFF-2'"),
    (2, "alice", "accepted", 10, "'PHID-DIFF-3'"),
    (3, "alice", "abandoned", 20, "'PHID-DIFF-4'"),
    // No active diff: left out of the results.
    (4, "alice", "needs-review", 40, "''"),
];

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
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy, branch, sourcePath) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-bob', 0, 0, 1, 1, 1, 'users', NULL, NULL), \
             (2, 'PHID-DIFF-2', 1, 'PHID-USER-bob', 0, 0, 1, 1, 1, 'users', 'main', '/src'), \
             (3, 'PHID-DIFF-3', 2, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users', NULL, '/a'), \
             (4, 'PHID-DIFF-4', 3, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users', NULL, NULL); \
             INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, \
               reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-carol', 'accepted', 1, 1, '{{}}'), \
             ('PHID-DREV-2', 'PHID-USER-bob', 'accepted', 1, 1, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.differential_revisionhash (revisionID, type, hash) \
               VALUES (1, 'hgcm', '0123456789abcdef0123456789abcdef01234567'); \
             INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 21, 'PHID-USER-dave', 1, 0), \
             ('PHID-DREV-1', 31, 'PHID-CMIT-1', 1, 0), \
             ('PHID-DREV-1', 41, 'PHID-PROJ-1', 1, 0)"
        ))
        .await;
    for (id, author, status, modified, diff) in REVISIONS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, \
                   status, summary, testPlan, authorPHID, dateCreated, dateModified, attached, \
                   mailKey, viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID, \
                   lineCount) VALUES \
                 ({id}, 'R{id}', 'PHID-DREV-{id}', '{status}', 'S', 'T', 'PHID-USER-{author}', \
                   {id}, {modified}, '{{}}', 'k', 'users', 'users', NULL, \
                   '{{\"lines.added\":2}}', {diff}, 7)"
            ))
            .await;
    }
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

#[tokio::test]
async fn describes_revisions() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let result = answer(&context, json!({"ids": [1]})).await;
    // Integers are strings; only the author (Bob) sees the source path.
    assert_eq!(
        result,
        json!([{
            "id": "1",
            "phid": "PHID-DREV-1",
            "title": "R1",
            "uri": "http://phabricator.test/D1",
            "dateCreated": "1",
            "dateModified": "30",
            "authorPHID": "PHID-USER-bob",
            "status": "0",
            "statusName": "Needs Review",
            "properties": {"lines.added": 2},
            "branch": "main",
            "summary": "S",
            "testPlan": "T",
            "lineCount": "7",
            "activeDiffPHID": "PHID-DIFF-2",
            "diffs": ["2", "1"],
            "commits": ["PHID-CMIT-1"],
            "reviewers": {"PHID-USER-carol": "PHID-USER-carol"},
            "ccs": ["PHID-USER-dave"],
            "hashes": [["hgcm", "0123456789abcdef0123456789abcdef01234567"]],
            "auxiliary": {
                "bugzilla.bug-id": null,
                "uplift.request": [],
                "phabricator:projects": ["PHID-PROJ-1"],
                "phabricator:depends-on": [],
            },
            "repositoryPHID": null,
            "sourcePath": "/src",
        }])
    );
    let other = answer(&context, json!({"ids": [2]})).await;
    assert!(other[0].get("sourcePath").is_none());
    assert_eq!(other[0]["status"], "2");

    test_db.drop_databases().await;
}

#[tokio::test]
async fn filters_orders_and_pages() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // By update by default; revisions without an active diff are skipped.
    assert_eq!(ids(&context, json!({})).await, ["1", "3", "2"]);
    assert_eq!(
        ids(&context, json!({"order": "order-created"})).await,
        ["3", "2", "1"]
    );
    assert_eq!(
        ids(&context, json!({"status": "status-open"})).await,
        ["1", "2"]
    );
    assert_eq!(
        ids(&context, json!({"status": "status-closed"})).await,
        ["3"]
    );
    assert_eq!(
        ids(&context, json!({"authors": ["PHID-USER-alice"]})).await,
        ["3", "2"]
    );
    assert_eq!(
        ids(&context, json!({"reviewers": ["PHID-USER-bob"]})).await,
        ["2"]
    );
    assert_eq!(
        ids(&context, json!({"responsibleUsers": ["PHID-USER-bob"]})).await,
        ["1", "2"]
    );
    assert_eq!(
        ids(&context, json!({"ccs": ["PHID-USER-dave"]})).await,
        ["1"]
    );
    // The offset counts the revisions without an active diff, which the
    // results skip after: D4 is the one skipped here.
    assert_eq!(ids(&context, json!({"limit": 1, "offset": 1})).await, ["1"]);
    assert_eq!(ids(&context, json!({"limit": 1, "offset": 2})).await, ["3"]);
    let Response::Envelope(envelope) = query(&context, json!({"status": "bogus"})).await else {
        panic!("expected an error");
    };
    assert_eq!(envelope.error_code.as_deref(), Some("ERR-CONDUIT-CORE"));
    assert!(matches!(
        query(&context, json!({"branches": ["main"]})).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}

async fn get_revision(context: &Context, params: Value) -> Response {
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
        "differential.getrevision",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

#[tokio::test]
async fn describes_one_revision() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let Response::Envelope(envelope) = get_revision(&context, json!({"revision_id": "2"})).await
    else {
        panic!("expected an answer");
    };
    let result = envelope.result;
    assert_eq!(
        result["diffs"]
            .as_object()
            .expect("a map")
            .keys()
            .collect::<Vec<_>>(),
        ["3"]
    );
    assert_eq!(result["diffs"]["3"]["id"], "3");
    let mut summary = result.clone();
    summary["diffs"] = Value::Null;
    assert_eq!(
        summary,
        json!({
            "id": "2",
            "phid": "PHID-DREV-2",
            "authorPHID": "PHID-USER-alice",
            "uri": "http://phabricator.test/D2",
            "title": "R2",
            "status": 2,
            "statusName": "Accepted",
            "summary": "S",
            "testPlan": "T",
            "lineCount": "7",
            "reviewerPHIDs": {"PHID-USER-bob": "PHID-USER-bob"},
            "diffs": null,
            "commits": [],
            "auxiliary": {
                "bugzilla.bug-id": null,
                "uplift.request": [],
                "phabricator:projects": [],
                "phabricator:depends-on": [],
            },
        })
    );
    let Response::Envelope(envelope) = get_revision(&context, json!({"revision_id": 99})).await
    else {
        panic!("expected an error");
    };
    assert_eq!(envelope.error_code.as_deref(), Some("ERR_BAD_REVISION"));
    // Revision 1's commit doesn't exist: its handle is PHP's.
    assert!(matches!(
        get_revision(&context, json!({"revision_id": 1})).await,
        Response::Fallback(_)
    ));

    // A rewritten commit is named after its new identifier.
    test_db
        .execute(
            "INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-1', 'Repo', 'R', 'git', '{}', 1, 1, 'users', 'admin', 'admin'); \
             INSERT INTO `{ns}_repository`.repository_commit (id, repositoryID, phid, \
               commitIdentifier, epoch, auditStatus, importStatus, summary) VALUES \
             (1, 1, 'PHID-CMIT-1', '1111111111111111111111111111111111111111', 500, 'none', \
               3083, 'Fix it'); \
             INSERT INTO `{ns}_repository`.repository_commithint (repositoryPHID, \
               oldCommitIdentifier, newCommitIdentifier, hintType) VALUES \
             ('PHID-REPO-1', '1111111111111111111111111111111111111111', \
               '2222222222222222222222222222222222222222', 'rewritten')",
        )
        .await;
    let Response::Envelope(envelope) = get_revision(&context, json!({"revision_id": 1})).await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        envelope.result["commits"],
        json!([{"fullname": "rR111111111111 \u{267B} rR222222222222: Fix it", "dateCommitted": "500"}])
    );

    test_db.drop_databases().await;
}
