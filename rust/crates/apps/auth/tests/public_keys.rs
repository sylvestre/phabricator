//! `auth.querypublickeys` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-pkpkpkpkpkpkpkpkpkpkpkpkpkpk";

async fn query(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_auth::register_queries(&mut registry);
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
        "auth.querypublickeys",
        &http,
        Mode::Serve,
        1,
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

fn ids(result: &Value) -> Vec<&str> {
    result["data"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|key| key["id"].as_str().expect("an ID"))
        .collect()
}

#[tokio::test]
async fn lists_active_keys_of_users() {
    let Some(test_db) =
        TestDatabase::create(&["auth", "user", "conduit", "policy", "spaces", "project"]).await
    else {
        return;
    };
    let index = phab_core::hash::digest_for_index(b"AAAAbody");
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_auth`.auth_sshkey (id, phid, objectPHID, name, keyType, keyBody, \
               keyComment, dateCreated, dateModified, keyIndex, isTrusted, isActive) VALUES \
             (1, 'PHID-AKEY-1', 'PHID-USER-bob', 'laptop', 'ssh-ed25519', 'AAAAbody', \
               'bob@laptop', 1, 1, '{index}', 0, 1), \
             (2, 'PHID-AKEY-2', 'PHID-USER-bob', 'old', 'ssh-rsa', 'AAAAold', '', 1, 1, \
               'oldoldoldold', 1, NULL), \
             (3, 'PHID-AKEY-3', 'PHID-USER-gone', 'gone', 'ssh-rsa', 'AAAAgone', '', 1, 1, \
               'gonegonegone', 0, 1), \
             (4, 'PHID-AKEY-4', 'PHID-USER-bob', 'ci', 'ssh-rsa', 'AAAAci', '', 1, 1, \
               'cicicicicici', 1, 1)"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };

    let result = answer(&context, json!({})).await;
    assert_eq!(
        result,
        json!({
            "data": [
                {"id": "4", "name": "ci", "phid": "PHID-AKEY-4", "objectPHID": "PHID-USER-bob",
                 "isTrusted": true, "key": "ssh-rsa AAAAci"},
                {"id": "1", "name": "laptop", "phid": "PHID-AKEY-1",
                 "objectPHID": "PHID-USER-bob", "isTrusted": false,
                 "key": "ssh-ed25519 AAAAbody bob@laptop"},
            ],
            "cursor": {"limit": 100, "after": null, "before": null},
        })
    );
    let result = answer(&context, json!({"limit": 1})).await;
    assert_eq!(ids(&result), ["4"]);
    assert_eq!(result["cursor"]["after"], json!("4"));
    let result = answer(&context, json!({"limit": 1, "after": "4"})).await;
    assert_eq!(ids(&result), ["1"]);
    let result = answer(&context, json!({"keys": ["ssh-ed25519 AAAAbody== other"]})).await;
    assert_eq!(ids(&result), ["1"]);
    let result = answer(
        &context,
        json!({"objectPHIDs": ["PHID-USER-bob"], "ids": [1, "2"]}),
    )
    .await;
    assert_eq!(ids(&result), ["1"]);
    assert_eq!(
        query(&context, json!({"keys": ["ssh-foo AAAA"]})).await,
        Response::Envelope(Envelope::error(
            "ERR-CONDUIT-CORE",
            "Public key type should be one of: ssh-dsa, ssh-dss, ssh-rsa, ssh-ed25519, \
             ecdsa-sha2-nistp256, ecdsa-sha2-nistp384, ecdsa-sha2-nistp521"
        ))
    );

    test_db.drop_databases().await;
}
