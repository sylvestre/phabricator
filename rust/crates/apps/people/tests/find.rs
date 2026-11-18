//! `user.find` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-fifififififififififififififi";

async fn find(context: &Context, params: Value) -> Response {
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
        "user.find",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn answer(context: &Context, params: Value) -> Value {
    match find(context, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{params}: expected an answer, got {other:?}"),
    }
}

#[tokio::test]
async fn maps_usernames_to_phids() {
    let Some(test_db) =
        TestDatabase::create(&["user", "conduit", "project", "policy", "spaces"]).await
    else {
        return;
    };
    for (id, name) in [(1, "carol"), (2, "bob")] {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_user`.user (id, phid, userName, realName, dateCreated, \
                   dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
                   isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
                   isMailingList) VALUES \
                 ({id}, 'PHID-USER-{name}', '{name}', '{name}', 1, 1, 'c', 0, 0, 0, 1, 1, 's', \
                   0, 0)"
            ))
            .await;
    }
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1)"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };

    // Newest first, keyed by the stored username.
    let result = answer(&context, json!({"aliases": ["carol", "BOB", "nobody"]})).await;
    assert_eq!(
        result,
        json!({"bob": "PHID-USER-bob", "carol": "PHID-USER-carol"})
    );
    assert_eq!(
        result
            .as_object()
            .expect("a map")
            .keys()
            .collect::<Vec<_>>(),
        ["bob", "carol"]
    );
    assert_eq!(
        answer(&context, json!({"aliases": ["nobody"]})).await,
        json!([])
    );
    assert!(matches!(
        find(&context, json!({"aliases": []})).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}
