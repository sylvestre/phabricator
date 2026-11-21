//! `flag.query` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-flflflflflflflflflflflflflfl";

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

/// `(id, owner, type, object, color, note)`.
const FLAGS: &[(i64, &str, &str, &str, i64, &str)] = &[
    (1, "bob", "USER", "PHID-USER-alice", 0, "Check her"),
    (2, "bob", "REPO", "PHID-REPO-open", 4, ""),
    // Alice's flag is private.
    (3, "alice", "USER", "PHID-USER-bob", 3, "Bob"),
    (4, "bob", "USER", "PHID-USER-bob", 9, "Me"),
];

async fn query(context: &Context, params: Value) -> Response {
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
    call::dispatch(
        context,
        &registry,
        "flag.query",
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

/// The IDs of the flags found, in order.
async fn ids(context: &Context, params: Value) -> Vec<String> {
    answer(context, params)
        .await
        .as_array()
        .expect("a list")
        .iter()
        .map(|flag| flag["id"].as_str().expect("id").to_owned())
        .collect()
}

async fn insert_flag(test_db: &TestDatabase, flag: (i64, &str, &str, &str, i64, &str)) {
    let (id, owner, flag_type, object, color, note) = flag;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_flag`.flag (id, ownerPHID, type, objectPHID, reasonPHID, color, \
               note, dateCreated, dateModified) VALUES \
             ({id}, 'PHID-USER-{owner}', '{flag_type}', '{object}', 'PHID-USER-{owner}', {color}, \
               '{note}', {id}0, {id}1)"
        ))
        .await;
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
               'admin')"
        ))
        .await;
    for flag in FLAGS {
        insert_flag(&test_db, *flag).await;
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
async fn lists_the_viewers_flags() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let all = answer(&context, json!({})).await;
    assert_eq!(
        all,
        json!([
            {
                "id": "4",
                "ownerPHID": "PHID-USER-bob",
                "type": "USER",
                "objectPHID": "PHID-USER-bob",
                "reasonPHID": "PHID-USER-bob",
                "color": "9",
                "colorName": "Unknown",
                "note": "Me",
                "handle": {
                    "uri": "http://phabricator.test/p/bob/",
                    "name": "bob",
                    "fullname": "bob (Bob)",
                },
                "dateCreated": "40",
                "dateModified": "41",
            },
            {
                "id": "2",
                "ownerPHID": "PHID-USER-bob",
                "type": "REPO",
                "objectPHID": "PHID-REPO-open",
                "reasonPHID": "PHID-USER-bob",
                "color": "4",
                "colorName": "Blue",
                "note": "",
                "handle": {
                    "uri": "http://phabricator.test/diffusion/OPEN/",
                    "name": "rOPEN",
                    "fullname": "rOPEN Open",
                },
                "dateCreated": "20",
                "dateModified": "21",
            },
            {
                "id": "1",
                "ownerPHID": "PHID-USER-bob",
                "type": "USER",
                "objectPHID": "PHID-USER-alice",
                "reasonPHID": "PHID-USER-bob",
                "color": "0",
                "colorName": "Red",
                "note": "Check her",
                "handle": {
                    "uri": "http://phabricator.test/p/alice/",
                    "name": "alice",
                    "fullname": "alice (Alice)",
                },
                "dateCreated": "10",
                "dateModified": "11",
            },
        ])
    );
    assert_eq!(ids(&context, json!({"types": ["REPO"]})).await, ["2"]);
    assert_eq!(
        ids(
            &context,
            json!({"objectPHIDs": ["PHID-USER-alice", "PHID-USER-bob"]})
        )
        .await,
        ["4", "1"]
    );
    assert_eq!(
        answer(&context, json!({"ownerPHIDs": ["PHID-USER-alice"]})).await,
        json!([])
    );
    assert_eq!(
        ids(
            &context,
            json!({"ownerPHIDs": ["PHID-USER-bob"], "types": [], "objectPHIDs": null})
        )
        .await,
        ["4", "2", "1"]
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn pages_with_offsets() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(ids(&context, json!({"limit": 1})).await, ["4"]);
    assert_eq!(
        ids(&context, json!({"limit": "1", "offset": "1"})).await,
        ["2"]
    );
    // Alice's flag is skipped, and is not counted by the offset.
    assert_eq!(ids(&context, json!({"limit": 0, "offset": 2})).await, ["1"]);
    assert_eq!(answer(&context, json!({"offset": 5})).await, json!([]));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_other_handles_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    insert_flag(&test_db, (5, "bob", "REPO", "PHID-REPO-secret", 1, "")).await;
    insert_flag(&test_db, (6, "bob", "TASK", "PHID-TASK-x", 1, "")).await;
    for params in [
        json!({}),
        json!({"types": ["REPO"]}),
        json!({"types": ["TASK"]}),
        json!({"ownerPHIDs": "PHID-USER-bob"}),
        json!({"types": [1]}),
        json!({"limit": -5}),
    ] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }
    // Flags 5 and 6 are filtered out.
    assert_eq!(
        ids(&context, json!({"objectPHIDs": ["PHID-REPO-open"]})).await,
        ["2"]
    );

    test_db.drop_databases().await;
}
