//! `user.query` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-uuuuuuuuuuuuuuuuuuuuuuuuuuuu";

/// The profile image cache entry PHP writes for the default avatar, with
/// the cache version of `http://phabricator.test/` (see `whoami.rs`).
const IMAGE: &str =
    "fQDBd9WAdWy_,http://phabricator.test/res/phabricator/3eb28cd9/rsrc/image/avatar.png";

fn config() -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "http://phabricator.test/"}) {
        config.push_layer("test", map);
    }
    config
}

async fn query(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_people::register(&mut registry);
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
        "user.query",
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

async fn names(context: &Context, params: Value) -> Vec<String> {
    answer(context, params)
        .await
        .as_array()
        .expect("a list")
        .iter()
        .map(|user| user["userName"].as_str().expect("userName").to_owned())
        .collect()
}

/// Users `(id, name, real name, availability cache, its TTL)`.
const USERS: &[(i64, &str, &str, &str, i64)] = &[
    // Back at 1700009999, as the valid cache says.
    (1, "carol", "Carol", "{\"until\":1700009999}", 1_700_005_000),
    // Expired cache, no invitations: available.
    (2, "bob", "Bob", "{\"until\":null}", 1),
    // Expired cache, invited to an event: PHP rebuilds the cache.
    (3, "alice", "Alice", "{\"until\":null}", 1),
];

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db =
        TestDatabase::create(&["user", "conduit", "project", "policy", "spaces", "calendar"])
            .await?;
    for (id, name, real_name, cache, ttl) in USERS {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_user`.user (id, phid, userName, realName, dateCreated, \
                   dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
                   isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
                   isMailingList, availabilityCache, availabilityCacheTTL) VALUES \
                 ({id}, 'PHID-USER-{name}', '{name}', '{real_name}', 1, 1, 'c', 0, 0, 0, 1, 1, \
                   's', 0, 0, '{cache}', {ttl}); \
                 INSERT INTO `{{ns}}_user`.user_email (phid, userPHID, address, isVerified, \
                   isPrimary, verificationCode, dateCreated, dateModified) VALUES \
                 ('PHID-EADR-{name}', 'PHID-USER-{name}', '{name}@example.com', 1, 1, 'v', 1, 1); \
                 INSERT INTO `{{ns}}_user`.user_cache (userPHID, cacheIndex, cacheKey, \
                   cacheData, cacheType) VALUES ('PHID-USER-{name}', 'y_FPXujT93U3', \
                   'user.profile.image.uri.v1', '{IMAGE}', 'user.profile')"
            ))
            .await;
    }
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_calendar`.calendar_eventinvitee (eventPHID, inviteePHID, \
               inviterPHID, status, dateCreated, dateModified, availability) VALUES \
             ('PHID-CEVT-1', 'PHID-USER-alice', 'PHID-USER-bob', 'attending', 1, 1, 'busy')"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: config(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn describes_users() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(
        answer(&context, json!({"usernames": ["carol", "bob"]})).await,
        json!([
            {
                "phid": "PHID-USER-bob",
                "userName": "bob",
                "realName": "Bob",
                "image": "http://phabricator.test/res/phabricator/3eb28cd9/rsrc/image/avatar.png",
                "uri": "http://phabricator.test/p/bob/",
                "roles": ["verified", "approved", "activated"],
            },
            {
                "phid": "PHID-USER-carol",
                "userName": "carol",
                "realName": "Carol",
                "image": "http://phabricator.test/res/phabricator/3eb28cd9/rsrc/image/avatar.png",
                "uri": "http://phabricator.test/p/carol/",
                "roles": ["verified", "approved", "activated"],
                "currentStatus": "away",
                "currentStatusUntil": 1_700_009_999,
            },
        ])
    );
    // Alice's availability comes from a calendar event.
    assert!(matches!(
        query(&context, json!({"usernames": ["alice"]})).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn filters_and_pages() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let only = |names: &[&str]| -> Vec<String> { names.iter().map(|&n| n.to_owned()).collect() };
    assert_eq!(
        names(&context, json!({"emails": ["carol@example.com"]})).await,
        only(&["carol"])
    );
    assert_eq!(
        names(&context, json!({"realnames": ["Bob"], "ids": [2]})).await,
        only(&["bob"])
    );
    assert_eq!(
        names(
            &context,
            json!({"phids": ["PHID-USER-carol", "PHID-USER-bob"]})
        )
        .await,
        only(&["bob", "carol"])
    );
    // Newest first; the limit and offset cut that list.
    assert_eq!(
        names(&context, json!({"ids": [1, 2], "limit": 1, "offset": 1})).await,
        only(&["carol"])
    );
    assert_eq!(
        names(&context, json!({"usernames": ["nobody"]})).await,
        only(&[])
    );

    test_db.drop_databases().await;
}
