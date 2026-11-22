//! `flag.edit` and `flag.delete` against a test database.

// One database, many scenarios: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-flededededededededededededed";
const NOW: i64 = 1_700_000_000;

const DATABASES: &[&str] = &[
    "flag",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
];

async fn call(context: &Context, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_flag::register(&mut registry);
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

async fn error(context: &Context, method: &str, params: Value) -> (String, String) {
    match call(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info: Some(info),
            ..
        }) => (code, info),
        other => panic!("{method} {params}: expected an error, got {other:?}"),
    }
}

/// The flags, as `id owner type object reason color note created modified`.
async fn flags(test_db: &TestDatabase) -> Vec<String> {
    let sql = format!(
        "SELECT CONCAT_WS(' ', id, ownerPHID, type, objectPHID, reasonPHID, color, note, \
           dateCreated, dateModified) AS line FROM `{}_flag`.flag ORDER BY id",
        test_db.database().namespace()
    );
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .expect("the flags")
        .iter()
        .map(|row| phab_db::rows::text(row, "line").expect("a line"))
        .collect()
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-open', 'Open', 'OPEN', 'git', '{{}}', 1, 1, 'users', 'admin', \
               'admin'), \
             (2, 'PHID-REPO-secret', 'Secret', 'SEC', 'git', '{{}}', 1, 1, 'admin', 'admin', \
               'admin'); \
             INSERT INTO `{{ns}}_flag`.flag (id, ownerPHID, type, objectPHID, reasonPHID, color, \
               note, dateCreated, dateModified) VALUES \
             (1, 'PHID-USER-bob', 'USER', 'PHID-USER-alice', 'PHID-USER-bob', 0, 'Check her', \
               10, 11), \
             (3, 'PHID-USER-alice', 'USER', 'PHID-USER-bob', 'PHID-USER-alice', 3, 'Bob', 30, 31)"
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
async fn edits_and_deletes_flags_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let alice_handle = json!({
        "uri": "http://phabricator.test/p/alice/",
        "name": "alice",
        "fullname": "alice (Alice)",
    });
    let repository_handle = json!({
        "uri": "http://phabricator.test/diffusion/OPEN/",
        "name": "rOPEN",
        "fullname": "rOPEN Open",
    });

    // A new flag: blue, without a note, and Lisk's integers.
    assert_eq!(
        answer(
            &context,
            "flag.edit",
            json!({"objectPHID": "PHID-REPO-open"})
        )
        .await,
        json!({
            "id": 4,
            "ownerPHID": "PHID-USER-bob",
            "type": "REPO",
            "objectPHID": "PHID-REPO-open",
            "reasonPHID": "PHID-USER-bob",
            "color": 4,
            "colorName": "Blue",
            "note": "",
            "handle": repository_handle,
            "dateCreated": NOW,
            "dateModified": NOW,
            "new": true,
        })
    );
    // Flagging again keeps what is not given, and MySQL's strings.
    assert_eq!(
        answer(
            &context,
            "flag.edit",
            json!({"objectPHID": "PHID-USER-alice", "color": "2", "note": null})
        )
        .await,
        json!({
            "id": "1",
            "ownerPHID": "PHID-USER-bob",
            "type": "USER",
            "objectPHID": "PHID-USER-alice",
            "reasonPHID": "PHID-USER-bob",
            "color": "2",
            "colorName": "Yellow",
            "note": "Check her",
            "handle": alice_handle,
            "dateCreated": "10",
            "dateModified": NOW,
            "new": false,
        })
    );
    assert_eq!(
        answer(
            &context,
            "flag.edit",
            json!({"objectPHID": "PHID-USER-alice", "note": "Again"})
        )
        .await["color"],
        "2"
    );
    assert_eq!(
        flags(&test_db).await,
        [
            "1 PHID-USER-bob USER PHID-USER-alice PHID-USER-bob 2 Again 10 1700000000",
            "3 PHID-USER-alice USER PHID-USER-bob PHID-USER-alice 3 Bob 30 31",
            "4 PHID-USER-bob REPO PHID-REPO-open PHID-USER-bob 4  1700000000 1700000000",
        ]
    );

    // Left to PHP, before anything is written.
    for params in [
        json!({"objectPHID": "PHID-REPO-secret"}),
        json!({"objectPHID": "PHID-TASK-unknown"}),
        json!({"objectPHID": "PHID-USER-alice", "color": "red"}),
        json!({"objectPHID": "PHID-USER-alice", "color": -1}),
        json!({"objectPHID": "PHID-USER-alice", "note": 5}),
        json!({"objectPHID": "PHID-USER-bob", "color": null}),
        json!({}),
    ] {
        assert!(
            matches!(
                call(&context, "flag.edit", params.clone()).await,
                Response::Fallback(_)
            ),
            "{params}"
        );
    }
    assert_eq!(flags(&test_db).await.len(), 3);

    // Errors.
    assert_eq!(
        error(&context, "flag.delete", json!({"id": 3})).await,
        (
            "ERR_WRONG_USER".to_owned(),
            "You are not the creator of this flag.".to_owned()
        )
    );
    assert_eq!(
        error(&context, "flag.delete", json!({"id": "99"})).await,
        ("ERR_NOT_FOUND".to_owned(), "Bad flag ID.".to_owned())
    );
    assert_eq!(
        error(&context, "flag.delete", json!({"id": 0, "objectPHID": ""})).await,
        (
            "ERR_NEED_PARAM".to_owned(),
            "Must pass an id or an objectPHID.".to_owned()
        )
    );
    // Nothing to delete: null.
    assert_eq!(
        answer(
            &context,
            "flag.delete",
            json!({"objectPHID": "PHID-USER-bob"})
        )
        .await,
        Value::Null
    );

    // Deleting answers the flag as it was.
    assert_eq!(
        answer(&context, "flag.delete", json!({"id": "1"})).await,
        json!({
            "id": "1",
            "ownerPHID": "PHID-USER-bob",
            "type": "USER",
            "objectPHID": "PHID-USER-alice",
            "reasonPHID": "PHID-USER-bob",
            "color": "2",
            "colorName": "Yellow",
            "note": "Again",
            "handle": alice_handle,
            "dateCreated": "10",
            "dateModified": "1700000000",
        })
    );
    assert_eq!(
        answer(
            &context,
            "flag.delete",
            json!({"objectPHID": "PHID-REPO-open"})
        )
        .await["id"],
        "4"
    );
    assert_eq!(
        flags(&test_db).await,
        ["3 PHID-USER-alice USER PHID-USER-bob PHID-USER-alice 3 Bob 30 31"]
    );

    test_db.drop_databases().await;
}
