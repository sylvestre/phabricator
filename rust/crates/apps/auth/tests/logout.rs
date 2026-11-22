//! `auth.logout` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::Value;

const TOKEN: &str = "api-aaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn sessions(test_db: &TestDatabase, user: &str) -> i64 {
    phab_db::sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {} WHERE userPHID = ?",
        test_db.database().table("user", "phabricator_session")
    ))
    .bind(user)
    .fetch_one(test_db.database().pool())
    .await
    .unwrap()
}

async fn logout(context: &Context, mode: Mode) -> Response {
    let mut registry = Registry::default();
    phab_app_auth::register(&mut registry);
    let http = HttpRequest {
        body: format!("api.token={TOKEN}"),
        ..HttpRequest::default()
    };
    call::dispatch(context, &registry, "auth.logout", &http, mode, 1)
        .await
        .response
}

#[tokio::test]
async fn terminates_every_session_of_the_caller() {
    let Some(test_db) =
        TestDatabase::create(&["user", "auth", "conduit", "project", "policy", "spaces"]).await
    else {
        return;
    };
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-alice', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', 'k1', 1, 2, 'PHID-SSSN-1', 0), \
             ('PHID-USER-alice', 'conduit', 'k2', 1, 2, 'PHID-SSSN-2', 0), \
             ('PHID-USER-bob', 'web', 'k3', 1, 2, 'PHID-SSSN-3', 0)"
        ))
        .await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web-1".to_owned(),
    };

    // Shadow calls answer like PHP without writing.
    assert_eq!(
        logout(&context, Mode::Shadow).await,
        Response::Envelope(Envelope::success(Value::Null))
    );
    assert_eq!(sessions(&test_db, "PHID-USER-alice").await, 2);

    assert_eq!(
        logout(&context, Mode::Serve).await,
        Response::Envelope(Envelope::success(Value::Null))
    );
    assert_eq!(sessions(&test_db, "PHID-USER-alice").await, 0);
    assert_eq!(sessions(&test_db, "PHID-USER-bob").await, 1);

    test_db.drop_databases().await;
}

#[tokio::test]
async fn requires_authentication() {
    let Some(test_db) =
        TestDatabase::create(&["user", "auth", "conduit", "project", "policy", "spaces"]).await
    else {
        return;
    };
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web-1".to_owned(),
    };
    let mut registry = Registry::default();
    phab_app_auth::register(&mut registry);
    let response = call::dispatch(
        &context,
        &registry,
        "auth.logout",
        &HttpRequest::default(),
        Mode::Serve,
        1,
    )
    .await
    .response;
    let Response::Envelope(envelope) = response else {
        panic!("expected an envelope, got {response:?}");
    };
    assert_eq!(
        envelope.to_body(),
        Envelope::error("ERR-INVALID-SESSION", "Session key is not present.").to_body()
    );
    test_db.drop_databases().await;
}
