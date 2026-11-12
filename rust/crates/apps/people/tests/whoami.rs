use phab_conduit::call::dispatch;
use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-bbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn config() -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "http://phabricator.test/"}) {
        config.push_layer("test", map);
    }
    config
}

async fn whoami(context: &Context) -> Response {
    let mut registry = Registry::default();
    phab_app_people::register(&mut registry);
    let http = HttpRequest {
        body: format!("api.token={TOKEN}"),
        ..HttpRequest::default()
    };
    dispatch(
        context,
        &registry,
        "user.whoami",
        &http,
        Mode::Serve,
        1_700_000_000,
    )
    .await
    .response
}

#[tokio::test]
async fn describes_the_caller() {
    let Some(test_db) =
        TestDatabase::create(&["user", "conduit", "project", "policy", "spaces"]).await
    else {
        return;
    };
    // The cache version is digestForIndex(serialize(array(
    // 'http://phabricator.test/', null, null))), computed by PHP.
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob <B>', 1, 1, 'c', 1, 0, 1, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_user`.user_email (userPHID, address, isVerified, isPrimary, \
               verificationCode, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'old@example.com', 1, 0, 'x', 1, 1), \
             ('PHID-USER-bob', 'bob@example.com', 1, 1, 'y', 1, 1); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1)"
        ))
        .await;

    let context = Context {
        database: test_db.database().clone(),
        config: config(),
        hostname: "web1".to_owned(),
    };

    // Without a valid cache entry, PHP builds it.
    assert_eq!(
        whoami(&context).await,
        Response::Fallback("profile image cache needs a rebuild")
    );
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user_cache (userPHID, cacheIndex, cacheKey, cacheData, cacheType) \
             VALUES ('PHID-USER-bob', 'y_FPXujT93U3', 'user.profile.image.uri.v1', \
               'stale,http://old/', 'user.profile')",
        )
        .await;
    assert!(matches!(whoami(&context).await, Response::Fallback(_)));
    test_db
        .execute(
            "UPDATE `{ns}_user`.user_cache SET cacheData = \
               'fQDBd9WAdWy_,http://phabricator.test/res/phabricator/3eb28cd9/rsrc/image/avatar.png'",
        )
        .await;

    let Response::Envelope(envelope) = whoami(&context).await else {
        panic!("expected an answer");
    };
    assert_eq!(
        envelope,
        Envelope::success(json!({
            "phid": "PHID-USER-bob",
            "userName": "bob",
            "realName": "Bob <B>",
            "image": "http://phabricator.test/res/phabricator/3eb28cd9/rsrc/image/avatar.png",
            "uri": "http://phabricator.test/p/bob/",
            "roles": ["agent", "admin", "verified", "approved", "activated"],
            "primaryEmail": "bob@example.com",
        }))
    );
    assert_eq!(
        envelope.to_body(),
        concat!(
            r#"{"result":{"phid":"PHID-USER-bob","userName":"bob","realName":"Bob \u003cB\u003e","#,
            r#""image":"http:\/\/phabricator.test\/res\/phabricator\/3eb28cd9\/rsrc\/image\/avatar.png","#,
            r#""uri":"http:\/\/phabricator.test\/p\/bob\/","roles":["agent","admin","verified","approved","activated"],"#,
            r#""primaryEmail":"bob@example.com"},"error_code":null,"error_info":null}"#
        )
    );

    test_db
        .execute("UPDATE `{ns}_user`.user_email SET isVerified = 0 WHERE isPrimary = 1")
        .await;
    let Response::Envelope(envelope) = whoami(&context).await else {
        panic!("expected an answer");
    };
    assert_eq!(envelope.result["primaryEmail"], Value::Null);
    assert_eq!(
        envelope.result["roles"],
        json!(["agent", "admin", "unverified", "approved", "activated"])
    );

    test_db.drop_databases().await;
}
