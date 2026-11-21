//! `audit.query` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-auauauauauauauauauauauauauau";

const DATABASES: &[&str] = &[
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
];

/// `(id, repository, audit status)`; repository 2 is secret.
const COMMITS: &[(i64, i64, &str)] = &[
    (1, 1, "needs-audit"),
    (2, 1, "audited"),
    (3, 1, "concern-raised"),
    (4, 2, "needs-audit"),
];

/// `(id, commit, auditor, status)`. The auditors of a commit sort like
/// their IDs, so the order of the rows does not depend on MySQL's plan.
const AUDITS: &[(i64, i64, &str, &str)] = &[
    (10, 1, "PHID-PROJ-team", "audit-required"),
    (11, 1, "PHID-USER-alice", "accepted"),
    (12, 2, "PHID-USER-alice", "accepted"),
    (13, 3, "PHID-USER-bob", "concern-raised"),
    (14, 4, "PHID-USER-alice", "audit-required"),
];

async fn query(context: &Context, params: Value) -> Response {
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
        "audit.query",
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

/// The IDs of the audit requests found, in order.
async fn ids(context: &Context, params: Value) -> Vec<String> {
    answer(context, params)
        .await
        .as_array()
        .expect("a list")
        .iter()
        .map(|audit| audit["id"].as_str().expect("id").to_owned())
        .collect()
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
             (1, 'PHID-REPO-open', 'Open', 'OPEN', 'git', '{{}}', 1, 1, 'users', 'admin', \
               'admin'), \
             (2, 'PHID-REPO-secret', 'Secret', 'SEC', 'git', '{{}}', 1, 1, 'admin', 'admin', \
               'admin')"
        ))
        .await;
    for (id, repository, status) in COMMITS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_repository`.repository_commit (id, repositoryID, phid, \
                   commitIdentifier, epoch, auditStatus, summary, importStatus) VALUES \
                   ({id}, {repository}, 'PHID-CMIT-{id}', '{id}{id}{id}{id}{id}{id}', {id}, \
                   '{status}', 'S{id}', 11)"
            ))
            .await;
    }
    for (id, commit, auditor, status) in AUDITS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_repository`.repository_auditrequest (id, auditorPHID, \
                   commitPHID, auditStatus, auditReasons) VALUES \
                   ({id}, '{auditor}', 'PHID-CMIT-{commit}', '{status}', '[]')"
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
async fn lists_audit_requests() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // Commits by decreasing ID; the secret repository's commit is hidden.
    let all = answer(&context, json!({})).await;
    assert_eq!(
        all,
        json!([
            {"id": "13", "commitPHID": "PHID-CMIT-3", "auditorPHID": "PHID-USER-bob",
             "reasons": [], "status": "concern-raised"},
            {"id": "12", "commitPHID": "PHID-CMIT-2", "auditorPHID": "PHID-USER-alice",
             "reasons": [], "status": "accepted"},
            {"id": "10", "commitPHID": "PHID-CMIT-1", "auditorPHID": "PHID-PROJ-team",
             "reasons": [], "status": "audit-required"},
            {"id": "11", "commitPHID": "PHID-CMIT-1", "auditorPHID": "PHID-USER-alice",
             "reasons": [], "status": "accepted"},
        ])
    );
    // Only the requested auditors' requests of their commits.
    assert_eq!(
        ids(&context, json!({"auditorPHIDs": ["PHID-USER-alice"]})).await,
        ["12", "11"]
    );
    assert_eq!(
        ids(
            &context,
            json!({"auditorPHIDs": ["PHID-USER-alice", "PHID-USER-nobody"], "limit": 1})
        )
        .await,
        ["12"]
    );
    assert_eq!(
        ids(&context, json!({"status": "audit-status-open"})).await,
        ["13", "10", "11"]
    );
    assert_eq!(
        ids(&context, json!({"status": "audit-status-accepted"})).await,
        ["12"]
    );
    assert_eq!(
        ids(&context, json!({"status": "audit-status-partial"})).await,
        Vec::<String>::new()
    );
    // Unknown statuses are no constraint.
    assert_eq!(
        ids(&context, json!({"status": "nope"})).await,
        ["13", "12", "10", "11"]
    );
    assert_eq!(
        ids(
            &context,
            json!({"commitPHIDs": ["PHID-CMIT-1", "PHID-CMIT-4"]})
        )
        .await,
        ["10", "11"]
    );
    assert_eq!(
        answer(&context, json!({"commitPHIDs": ["PHID-CMIT-4"]})).await,
        json!([])
    );
    assert_eq!(
        answer(&context, json!({"auditorPHIDs": ["PHID-USER-nobody"]})).await,
        json!([])
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn offsets_and_limits_count_commits() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(ids(&context, json!({"limit": 1})).await, ["13"]);
    assert_eq!(
        ids(&context, json!({"offset": 1, "limit": 1})).await,
        ["12"]
    );
    assert_eq!(
        ids(&context, json!({"offset": "2", "limit": "5"})).await,
        ["10", "11"]
    );
    // No limit.
    assert_eq!(
        ids(&context, json!({"limit": 0, "offset": 1})).await,
        ["12", "10", "11"]
    );
    assert_eq!(
        ids(&context, json!({"offset": 3})).await,
        Vec::<String>::new()
    );
    // Empty lists and null are no constraint.
    assert_eq!(
        ids(
            &context,
            json!({"auditorPHIDs": [], "commitPHIDs": null, "offset": null})
        )
        .await,
        ["13", "12", "10", "11"]
    );
    for params in [
        json!({"auditorPHIDs": ""}),
        json!({"auditorPHIDs": "PHID-USER-alice"}),
        json!({"commitPHIDs": [1]}),
        json!({"status": ["audit-status-open"]}),
        json!({"limit": -1}),
        json!({"offset": "x"}),
    ] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}
