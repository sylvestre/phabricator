//! `bugzilla.account.search` and `bmoexternalaccount.search` against a test
//! database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::{Config, hash};
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-dddddddddddddddddddddddddddd";
const BMO: &str = "PHID-AUTH-bmo";
const GITHUB: &str = "PHID-AUTH-github";

async fn query(context: &Context, method: &str, params: Value) -> Response {
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
        method,
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn answer(context: &Context, method: &str, params: Value) -> Value {
    match query(context, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

/// An external account of `provider` with `identifiers`, oldest first.
async fn add_account(
    test_db: &TestDatabase,
    id: u32,
    provider: &str,
    user: Option<&str>,
    identifiers: &[&str],
) {
    let user = user.map_or_else(|| "NULL".to_owned(), |user| format!("'{user}'"));
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user_externalaccount (id, phid, userPHID, accountType, \
               accountDomain, accountID, dateCreated, dateModified, emailVerified, properties, \
               providerConfigPHID) VALUES \
             ({id}, 'PHID-XUSR-{id}', {user}, 'bmo', '{provider}', '{id}', 1, 1, 0, '{{}}', \
               '{provider}')"
        ))
        .await;
    for raw in identifiers {
        let digest = hash::digest_for_index(raw.as_bytes());
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_user`.user_externalaccountidentifier (phid, \
                   externalAccountPHID, providerConfigPHID, identifierHash, identifierRaw, \
                   dateCreated, dateModified) VALUES \
                 ('PHID-XIDT-{id}-{raw}', 'PHID-XUSR-{id}', '{provider}', '{digest}', '{raw}', 1, 1)"
            ))
            .await;
    }
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db =
        TestDatabase::create(&["user", "auth", "conduit", "project", "policy", "spaces"]).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_auth`.auth_providerconfig (phid, providerClass, providerType, \
               providerDomain, isEnabled, shouldAllowLogin, shouldAllowRegistration, \
               shouldAllowLink, shouldAllowUnlink, properties, dateCreated, dateModified) VALUES \
             ('{BMO}', 'PhabricatorBMOAuthProvider', 'bmo', 'bmo.test', 1, 1, 1, 1, 1, '{{}}', 1, 1), \
             ('{GITHUB}', 'PhabricatorGitHubAuthProvider', 'github', 'github.com', 1, 1, 1, 1, 1, \
               '{{}}', 1, 1)"
        ))
        .await;
    add_account(&test_db, 1, BMO, Some("PHID-USER-alice"), &["101"]).await;
    // PHP reports the newest identifier.
    add_account(&test_db, 2, BMO, Some("PHID-USER-bob"), &["202", "203"]).await;
    // "0" is falsy, so PHP skips the account.
    add_account(&test_db, 3, BMO, None, &["0"]).await;
    // Same identifier, other provider.
    add_account(&test_db, 4, GITHUB, Some("PHID-USER-dave"), &["101"]).await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn maps_bugzilla_ids_and_users() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    assert_eq!(
        answer(
            &context,
            "bugzilla.account.search",
            json!({"ids": ["101", "203", "0"]})
        )
        .await,
        json!([
            {"id": "203", "phid": "PHID-USER-bob"},
            {"id": "101", "phid": "PHID-USER-alice"},
        ])
    );
    // Integers are hashed like their decimal strings.
    assert_eq!(
        answer(&context, "bugzilla.account.search", json!({"ids": [101]})).await,
        json!([{"id": "101", "phid": "PHID-USER-alice"}])
    );
    assert_eq!(
        answer(
            &context,
            "bugzilla.account.search",
            json!({"phids": ["PHID-USER-alice", "PHID-USER-dave"]})
        )
        .await,
        json!([{"id": "101", "phid": "PHID-USER-alice"}])
    );
    // ids win over phids, and nothing asked is nothing found.
    assert_eq!(
        answer(
            &context,
            "bugzilla.account.search",
            json!({"ids": ["999"], "phids": ["PHID-USER-bob"]})
        )
        .await,
        json!([])
    );
    assert_eq!(
        answer(
            &context,
            "bugzilla.account.search",
            json!({"ids": [], "phids": ""})
        )
        .await,
        json!([])
    );
    assert_eq!(
        answer(
            &context,
            "bmoexternalaccount.search",
            json!({"accountids": ["202"]})
        )
        .await,
        json!([{"id": "203", "phid": "PHID-USER-bob"}])
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_failures_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    for (method, params) in [
        ("bmoexternalaccount.search", json!({"accountids": []})),
        ("bmoexternalaccount.search", json!({})),
        ("bugzilla.account.search", json!({"ids": "101"})),
    ] {
        assert!(
            matches!(
                query(&context, method, params.clone()).await,
                Response::Fallback(_)
            ),
            "PHP fails on {method} {params}"
        );
    }

    add_account(&test_db, 5, BMO, Some("PHID-USER-carol"), &[]).await;
    assert!(matches!(
        query(
            &context,
            "bugzilla.account.search",
            json!({"phids": ["PHID-USER-carol"]})
        )
        .await,
        Response::Fallback(_)
    ));

    test_db
        .execute("DELETE FROM `{ns}_auth`.auth_providerconfig WHERE providerType = 'bmo'")
        .await;
    assert!(matches!(
        query(&context, "bugzilla.account.search", json!({"ids": ["101"]})).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}
