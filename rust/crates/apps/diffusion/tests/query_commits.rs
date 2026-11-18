//! `diffusion.querycommits` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-qcqcqcqcqcqcqcqcqcqcqcqcqcqc";

const DATABASES: &[&str] = &[
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
];

/// `(id, repository, hash, epoch, commit details)`; commit 3 has no data.
const COMMITS: &[(i64, i64, &str, i64, Option<&str>)] = &[
    (
        1,
        1,
        "1111111111111111111111111111111111111111",
        300,
        Some(
            r#"{"authorEpoch":"250","ref":{"authorName":"Alice","authorEmail":"alice@example.com","committerName":null},"committer":"Bot <bot@example.com>","committerPHID":"PHID-USER-bot"}"#,
        ),
    ),
    (
        2,
        1,
        "1122222222222222222222222222222222222222",
        900,
        Some(r#"{"authorName":"Bob"}"#),
    ),
    (3, 1, "3333333333333333333333333333333333333333", 100, None),
    // In a repository Bob can't see.
    (
        4,
        2,
        "4444444444444444444444444444444444444444",
        400,
        Some("{}"),
    ),
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
        "diffusion.querycommits",
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

/// The IDs of the commits found, in order.
async fn ids(context: &Context, params: Value) -> Vec<String> {
    match &answer(context, params).await["data"] {
        Value::Object(data) => data
            .values()
            .map(|commit| commit["id"].as_str().expect("id").to_owned())
            .collect(),
        _ => Vec::new(),
    }
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
    for (id, repository, hash, epoch, details) in COMMITS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_repository`.repository_commit (id, repositoryID, phid, \
                   commitIdentifier, epoch, auditStatus, summary, importStatus, authorPHID) \
                   VALUES ({id}, {repository}, 'PHID-CMIT-{id}', '{hash}', {epoch}, 'none', \
                   'S{id}', 11, 'PHID-USER-alice')"
            ))
            .await;
        if let Some(details) = details {
            test_db
                .execute(&format!(
                    "INSERT INTO `{{ns}}_repository`.repository_commitdata (commitID, \
                       authorName, commitMessage, commitDetails) VALUES \
                     ({id}, 'raw{id}', 'Message {id}', '{details}')"
                ))
                .await;
        }
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
async fn describes_commits() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let result = answer(&context, json!({"ids": [1, 2, 3], "needMessages": true})).await;
    assert_eq!(
        result["data"]["PHID-CMIT-1"],
        json!({
            "id": "1",
            "phid": "PHID-CMIT-1",
            "repositoryPHID": "PHID-REPO-open",
            "identifier": "1111111111111111111111111111111111111111",
            "epoch": "300",
            "authorEpoch": 250,
            "uri": "http://phabricator.test/rOPEN1111111111111111111111111111111111111111",
            "isImporting": false,
            "summary": "S1",
            "authorPHID": "PHID-USER-alice",
            "committerPHID": "PHID-USER-bot",
            "author": "Alice <alice@example.com>",
            "authorName": "Alice",
            "authorEmail": "alice@example.com",
            // The ref's null name wins; the detail is the committer.
            "committer": "Bot <bot@example.com>",
            "committerName": null,
            "committerEmail": null,
            "hashes": [],
            "message": "Message 1",
        })
    );
    // Without a ref, the details; without data, nothing.
    assert_eq!(result["data"]["PHID-CMIT-2"]["author"], "Bob");
    let empty = &result["data"]["PHID-CMIT-3"];
    assert_eq!(
        (&empty["author"], &empty["message"], &empty["authorEpoch"]),
        (&Value::Null, &Value::Null, &Value::Null)
    );
    assert_eq!(
        result["cursor"],
        json!({"limit": 100, "after": null, "before": null})
    );
    assert_eq!(result["identifierMap"], json!({}));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn finds_names_and_pages() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // By ID, not by date; the secret repository's commit is hidden.
    assert_eq!(ids(&context, json!({})).await, ["3", "2", "1"]);
    let first = answer(&context, json!({"limit": 2})).await;
    assert_eq!(
        first["cursor"],
        json!({"limit": 2, "after": "2", "before": null})
    );
    assert_eq!(
        ids(&context, json!({"limit": 2, "after": "2"})).await,
        ["1"]
    );
    // "11111" names one commit, "rOPEN11" is too short, "11" is ambiguous
    // as a prefix but too short anyway.
    let found = answer(
        &context,
        json!({"names": ["rOPEN11111", "R1:11222", "4444444444"]}),
    )
    .await;
    assert_eq!(
        found["identifierMap"],
        json!({"rOPEN11111": "PHID-CMIT-1", "R1:11222": "PHID-CMIT-2"})
    );
    assert_eq!(
        ids(&context, json!({"repositoryPHID": "PHID-REPO-nope"})).await,
        ["3", "2", "1"]
    );
    for params in [
        json!({"repositoryPHID": "PHID-REPO-secret"}),
        json!({"names": ["rOPEN11111", "rOPEN11222"], "limit": 1}),
        json!({"ids": 1}),
    ] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}
