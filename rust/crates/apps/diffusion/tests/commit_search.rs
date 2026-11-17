//! `diffusion.commit.search` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-cccccccccccccccccccccccccccc";

const DATABASES: &[&str] = &[
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
];

/// `(id, repository, hash, epoch, author identity, import status)`.
const COMMITS: &[(i64, i64, &str, i64, &str, i64)] = &[
    (
        1,
        1,
        "1111111111111111111111111111111111111111",
        300,
        "alice",
        1035,
    ),
    (
        2,
        1,
        "2222222222222222222222222222222222222222",
        100,
        "outsider",
        1035,
    ),
    // Rewritten away: IMPORTED_UNREACHABLE (2048) is set.
    (
        3,
        1,
        "3333333333333333333333333333333333333333",
        200,
        "alice",
        3083,
    ),
    // In a repository Bob can't see.
    (
        4,
        2,
        "4444444444444444444444444444444444444444",
        400,
        "alice",
        1035,
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
        "diffusion.commit.search",
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

async fn ids(context: &Context, params: Value) -> Vec<i64> {
    answer(context, params).await["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|commit| commit["id"].as_i64().expect("id"))
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
             (1, 'PHID-REPO-open', 'Open', 'OPEN', 'hg', '{{}}', 1, 1, 'users', 'admin', 'admin'), \
             (2, 'PHID-REPO-secret', 'Secret', 'SEC', 'hg', '{{}}', 1, 1, 'admin', 'admin', \
               'admin'); \
             INSERT INTO `{{ns}}_repository`.repository_identity (phid, dateCreated, \
               dateModified, identityNameHash, identityNameRaw, identityNameEncoding, authorPHID, \
               currentEffectiveUserPHID) VALUES \
             ('PHID-RIDT-alice', 1, 1, 'h1', 'Alice Liddell <alice@example.com>', 'utf8', \
               'PHID-USER-alice', 'PHID-USER-alice'), \
             ('PHID-RIDT-outsider', 1, 1, 'h2', 'outsider@nowhere.test', 'utf8', \
               'PHID-USER-x', NULL)"
        ))
        .await;
    for (id, repository, hash, epoch, author, status) in COMMITS {
        let user = if *author == "alice" {
            "'PHID-USER-alice'"
        } else {
            "NULL"
        };
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_repository`.repository_commit (id, repositoryID, phid, \
                   commitIdentifier, epoch, auditStatus, summary, importStatus, authorPHID, \
                   authorIdentityPHID) VALUES \
                 ({id}, {repository}, 'PHID-CMIT-{id}', '{hash}', {epoch}, 'none', 'S{id}', \
                   {status}, {user}, 'PHID-RIDT-{author}'); \
                 INSERT INTO `{{ns}}_repository`.repository_commitdata (commitID, authorName, \
                   commitMessage, commitDetails) VALUES \
                 ({id}, '{author}', 'Message {id}', '{{\"authorEpoch\":\"{epoch}\"}}')"
            ))
            .await;
    }
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn describes_commits() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let result = answer(
        &context,
        json!({"constraints": {"ids": [1, 2]}, "attachments": {"auditors": true}}),
    )
    .await;
    assert_eq!(
        result["data"][0],
        json!({
            "id": 1,
            "type": "CMIT",
            "phid": "PHID-CMIT-1",
            "fields": {
                "identifier": "1111111111111111111111111111111111111111",
                "repositoryPHID": "PHID-REPO-open",
                "author": {
                    "name": "Alice Liddell",
                    "email": "alice@example.com",
                    "raw": "Alice Liddell <alice@example.com>",
                    "epoch": 300,
                    "identityPHID": "PHID-RIDT-alice",
                    "userPHID": "PHID-USER-alice",
                },
                "committer": {
                    "name": null, "email": null, "raw": null, "epoch": 300,
                    "identityPHID": null, "userPHID": null,
                },
                "isUnreachable": false,
                "isImported": true,
                "auditStatus": {
                    "value": "none", "name": "No Audits", "closed": true, "color.ansi": null,
                },
                "message": "Message 1",
                "policy": {"view": "users", "edit": "users"},
            },
            "attachments": {"auditors": {"auditors": []}},
        })
    );
    // An address without a display name.
    assert_eq!(
        result["data"][1]["fields"]["author"],
        json!({
            "name": null,
            "email": "outsider@nowhere.test",
            "raw": "outsider@nowhere.test",
            "epoch": 100,
            "identityPHID": "PHID-RIDT-outsider",
            "userPHID": null,
        })
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn orders_and_pages() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    // By commit date by default; the secret repository's commit is hidden.
    assert_eq!(ids(&context, json!({})).await, [1, 3, 2]);
    assert_eq!(ids(&context, json!({"order": "oldest"})).await, [2, 3, 1]);
    assert_eq!(
        ids(&context, json!({"order": "importnew"})).await,
        [3, 2, 1]
    );
    assert_eq!(
        ids(&context, json!({"order": "importold"})).await,
        [1, 2, 3]
    );
    let first = answer(&context, json!({"limit": 2})).await;
    assert_eq!(first["cursor"]["after"], "3");
    assert_eq!(ids(&context, json!({"limit": 2, "after": "3"})).await, [2]);

    test_db.drop_databases().await;
}

#[tokio::test]
async fn constrains() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(
        ids(
            &context,
            json!({"constraints": {"authors": ["PHID-USER-alice"]}})
        )
        .await,
        [1, 3]
    );
    // Projects never write commits.
    assert_eq!(
        ids(
            &context,
            json!({"constraints": {"authors": ["PHID-PROJ-x"]}})
        )
        .await,
        [0_i64; 0]
    );
    assert_eq!(
        ids(&context, json!({"constraints": {"unreachable": true}})).await,
        [3]
    );
    assert_eq!(
        ids(&context, json!({"constraints": {"unreachable": false}})).await,
        [1, 2]
    );
    assert_eq!(
        ids(
            &context,
            json!({"constraints": {"repositories": ["PHID-REPO-secret"]}})
        )
        .await,
        [0_i64; 0]
    );
    assert_eq!(
        ids(&context, json!({"constraints": {"statuses": ["0"]}})).await,
        [1, 3, 2]
    );
    assert_eq!(
        ids(
            &context,
            json!({"constraints": {"identifiers": ["2222222", "rOPEN:33333", "rSEC44444"]}})
        )
        .await,
        [3, 2]
    );
    // Too short to name a commit.
    assert_eq!(
        ids(
            &context,
            json!({"constraints": {"identifiers": ["111111"]}})
        )
        .await,
        [0_i64; 0]
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_the_rest_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    for params in [
        json!({"order": "relevance"}),
        json!({"constraints": {"authors": ["alice"]}}),
        json!({"constraints": {"query": "fix"}}),
        json!({"constraints": {"ancestorsOf": ["default"]}}),
        json!({"constraints": {"responsible": ["PHID-USER-alice"]}}),
    ] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }
    // Audits are not ported.
    test_db
        .execute(
            "INSERT INTO `{ns}_repository`.repository_auditrequest (auditorPHID, commitPHID, \
               auditStatus, auditReasons) VALUES ('PHID-USER-bob', 'PHID-CMIT-1', 'audit-required', \
               '[]')",
        )
        .await;
    assert!(matches!(
        query(&context, json!({})).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}
