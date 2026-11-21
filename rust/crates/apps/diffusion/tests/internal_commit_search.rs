//! `internal.commit.search` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-icicicicicicicicicicicicicic";

const DATABASES: &[&str] = &[
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
        "internal.commit.search",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn envelope(context: &Context, params: Value) -> Envelope {
    match query(context, params.clone()).await {
        Response::Envelope(envelope) => envelope,
        other @ Response::Fallback(_) => panic!("{params}: expected an answer, got {other:?}"),
    }
}

/// The `ERR-CONDUIT-CORE` message of the call.
async fn core_error(context: &Context, params: Value) -> String {
    let envelope = envelope(context, params.clone()).await;
    assert_eq!(
        envelope.error_code.as_deref(),
        Some("ERR-CONDUIT-CORE"),
        "{params}"
    );
    envelope.error_info.expect("error_info")
}

/// The constraints naming one repository.
fn constraints(repository: &str) -> Value {
    json!({"repositoryPHIDs": [repository]})
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
               pushPolicy, almanacServicePHID) VALUES \
             (1, 'PHID-REPO-open', 'Open', 'OPEN', 'git', '{{}}', 1, 1, 'users', 'admin', \
               'admin', NULL), \
             (2, 'PHID-REPO-empty', 'Empty', 'EMPTY', 'git', '{{}}', 1, 1, 'users', 'admin', \
               'admin', NULL), \
             (3, 'PHID-REPO-secret', 'Secret', 'SEC', 'git', '{{}}', 1, 1, 'admin', 'admin', \
               'admin', NULL), \
             (4, 'PHID-REPO-cluster', 'Cluster', 'CLU', 'git', '{{}}', 1, 1, 'users', 'admin', \
               'admin', 'PHID-ASRV-x'); \
             INSERT INTO `{{ns}}_repository`.repository_commit (id, repositoryID, phid, \
               commitIdentifier, epoch, auditStatus, summary, importStatus) VALUES \
             (1, 1, 'PHID-CMIT-1', 'aaaaaaa', 10, 'none', 'S1', 11)"
        ))
        .await;
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
async fn needs_one_repository() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let message = "This internal method must be invoked with a \"repositoryPHIDs\" constraint \
                   with exactly one value.";
    for params in [
        json!({}),
        json!({"constraints": {}}),
        json!({"constraints": ["PHID-REPO-empty"]}),
        json!({"constraints": {"repositoryPHIDs": []}}),
        json!({"constraints": {"repositoryPHIDs": ["PHID-REPO-empty", "PHID-REPO-open"]}}),
        json!({"constraints": {"repositoryPHIDs": "PHID-REPO-empty"}}),
        // Checked before anything else.
        json!({"constraints": {"nope": 1}, "queryKey": "all"}),
    ] {
        assert_eq!(
            core_error(&context, params.clone()).await,
            message,
            "{params}"
        );
    }

    test_db.drop_databases().await;
}

#[tokio::test]
async fn searches_which_find_nothing() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let empty = |limit: Value, order: Value| {
        json!({
            "data": [],
            "maps": {},
            "query": {"queryKey": null},
            "cursor": {"limit": limit, "after": null, "before": null, "order": order},
        })
    };
    for repository in ["PHID-REPO-empty", "PHID-REPO-nope"] {
        assert_eq!(
            envelope(&context, json!({"constraints": constraints(repository)}))
                .await
                .result,
            empty(json!(100), Value::Null),
            "{repository}"
        );
    }
    assert_eq!(
        envelope(
            &context,
            json!({
                "constraints": constraints("PHID-REPO-empty"),
                "limit": "5",
                "order": "oldest",
                "after": "1",
                "attachments": {},
            })
        )
        .await
        .result,
        empty(json!("5"), json!("oldest"))
    );
    assert_eq!(
        core_error(
            &context,
            json!({"constraints": {"repositoryPHIDs": ["PHID-REPO-empty"], "nope": 1}})
        )
        .await,
        "Constraint \"nope\" is not a valid constraint for this query."
    );
    assert_eq!(
        core_error(
            &context,
            json!({"constraints": constraints("PHID-REPO-empty"), "order": "nope"})
        )
        .await,
        "Query \"DiffusionCommitQuery\" does not support a builtin order \"nope\". Supported \
         orders are: newest, oldest, relevance, importnew, created, importold."
    );
    assert_eq!(
        core_error(
            &context,
            json!({"constraints": constraints("PHID-REPO-empty"), "limit": 101})
        )
        .await,
        "Maximum page size for Conduit API method calls is 100, but this call specified 101."
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_commits_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    for params in [
        // The commit's ref comes from the working copy.
        json!({"constraints": constraints("PHID-REPO-open")}),
        json!({"constraints": constraints("PHID-REPO-secret")}),
        json!({"constraints": constraints("PHID-REPO-cluster")}),
        json!({"constraints": {"repositoryPHIDs": ["PHID-REPO-empty"], "ids": [1]}}),
        json!({"constraints": constraints("rEMPTY")}),
        json!({"constraints": constraints("PHID-REPO-empty"), "queryKey": "all"}),
        json!({"constraints": constraints("PHID-REPO-empty"), "attachments": {"x": true}}),
    ] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}
