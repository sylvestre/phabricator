//! The storage of the login workflow against a test database.

// Each test walks a whole workflow.
#![allow(clippy::too_many_lines)]

use phab_auth::external_account::{self, AccountProfile};
use phab_auth::password::{self, Password};
use phab_auth::provider::{self, ProviderKind};
use phab_auth::session::{self, SessionDigest, SessionType};
use phab_auth::user_log::RequestInfo;
use phab_auth::{Outcome, Writes, rate_limit};
use phab_core::{Config, Phid, hash};
use phab_db::testing::TestDatabase;
use serde_json::json;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
/// Made by PHP: `password_hash(hash_hmac('sha256', 'admin-password',
/// 'adminsalt'), PASSWORD_BCRYPT, ['cost' => 11])`.
const ADMIN_HASH: &str = "bcrypt:$2y$11$R/PqsbNSjpoD3K9VMfjc3OU.dEM.OTSqYxHLKLPEPU.poSAZwFfj.";

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&["user", "auth", "system"]).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-admin', 'admin', 'Admin', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1); \
             INSERT INTO `{{ns}}_auth`.auth_providerconfig (phid, providerClass, providerType, \
               providerDomain, isEnabled, shouldAllowLogin, shouldAllowRegistration, \
               shouldAllowLink, shouldAllowUnlink, shouldTrustEmails, properties, dateCreated, \
               dateModified, shouldAutoLogin) VALUES \
             ('PHID-AUTH-bmo', 'PhabricatorBMOAuthProvider', 'bmo', 'bugzilla.mozilla.org', \
               1, 1, 1, 1, 1, 0, '{{\"oauth:app:id\":\"client\",\"oauth:app:secret\":\"s3cret\"}}', \
               1, 1, 0), \
             ('PHID-AUTH-other', 'PhabricatorGitHubAuthProvider', 'github', 'github.com', \
               1, 1, 1, 1, 1, 0, '[]', 1, 1, 0); \
             INSERT INTO `{{ns}}_auth`.auth_password (phid, objectPHID, passwordType, \
               passwordHash, isRevoked, dateCreated, dateModified, passwordSalt, \
               legacyDigestFormat) VALUES \
             ('PHID-APAS-1', 'PHID-USER-admin', 'account', '{ADMIN_HASH}', 0, 1, 1, \
               'adminsalt', NULL), \
             ('PHID-APAS-2', 'PHID-USER-admin', 'account', 'bcrypt:revoked', 1, 1, 1, \
               'adminsalt', NULL)"
        ))
        .await;
    Some(test_db)
}

fn request() -> RequestInfo {
    RequestInfo {
        remote_address: "10.0.0.1".to_owned(),
        user_agent: None,
        hostname: "web-1".to_owned(),
    }
}

async fn text(test_db: &TestDatabase, sql: &str) -> String {
    let sql = format!(
        "SELECT CAST(({}) AS CHAR) AS v",
        sql.replace("{ns}", test_db.database().namespace())
    );
    let row = phab_db::sqlx::query(&sql)
        .fetch_one(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    phab_db::rows::text(&row, "v").unwrap()
}

#[tokio::test]
async fn sessions_are_issued_upgraded_and_ended_like_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let database = test_db.database();
    let Outcome::Done(keys) = SessionDigest::load(database).await.unwrap() else {
        panic!("no digest key");
    };
    let user = Phid::new("PHID-USER-admin");
    let issued = session::establish_session(
        database,
        &keys,
        SessionType::Web,
        &user,
        true,
        NOW,
        &request(),
    )
    .await
    .unwrap();
    assert!(!format!("{issued:?}").contains(issued.token()));
    assert_eq!(
        issued.digest,
        hash::hmac_sha256(
            issued.token().as_bytes(),
            &hex::decode(DIGEST_KEY_HEX).unwrap()
        )
        .unwrap()
    );
    assert_eq!(
        text(
            &test_db,
            "SELECT CONCAT_WS('|', type, sessionStart, sessionExpires, isPartial) \
             FROM `{ns}_user`.phabricator_session"
        )
        .await,
        format!("web|{NOW}|{}|1", NOW + 1800)
    );
    assert_eq!(
        text(
            &test_db,
            "SELECT CONCAT_WS('|', action, userPHID, IFNULL(actorPHID, '-'), details) \
             FROM `{ns}_user`.user_log"
        )
        .await,
        "login-partial|PHID-USER-admin|-|{\"session_type\":\"web\",\"host\":\"web-1\",\
         \"user_agent\":null}"
    );

    // The session PHP's `loadUserForSession()` would load.
    let Outcome::Done(Some(found)) = session::load_user_for_session(
        database,
        &Config::default(),
        SessionType::Web,
        issued.token(),
        NOW,
        Writes::Skipped,
    )
    .await
    .unwrap() else {
        panic!("the new session doesn't load");
    };
    assert!(found.session.is_partial);
    session::upgrade_partial_session(database, &found, NOW, &request())
        .await
        .unwrap();
    session::logout_session(database, &found, NOW, &request())
        .await
        .unwrap();
    assert_eq!(
        text(
            &test_db,
            "SELECT GROUP_CONCAT(action ORDER BY id) FROM `{ns}_user`.user_log"
        )
        .await,
        "login-partial,login-full,logout"
    );
    assert_eq!(
        text(
            &test_db,
            "SELECT COUNT(*) FROM `{ns}_user`.phabricator_session"
        )
        .await,
        "0"
    );

    // Conduit sessions last a day, partial or not.
    session::establish_session(
        database,
        &keys,
        SessionType::Conduit,
        &user,
        false,
        NOW,
        &request(),
    )
    .await
    .unwrap();
    assert_eq!(
        text(
            &test_db,
            "SELECT sessionExpires FROM `{ns}_user`.phabricator_session WHERE type = 'conduit'"
        )
        .await,
        (NOW + 86_400).to_string()
    );
    session::terminate_login_sessions(database, &user)
        .await
        .unwrap();
    assert_eq!(
        text(
            &test_db,
            "SELECT COUNT(*) FROM `{ns}_user`.phabricator_session"
        )
        .await,
        "0"
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn external_accounts_are_created_then_found_by_identifier() {
    let Some(test_db) = setup().await else {
        return;
    };
    let database = test_db.database();
    let Outcome::Done(providers) = provider::load_providers(database).await.unwrap() else {
        panic!("providers");
    };
    // The GitHub row has no installed class.
    assert_eq!(providers.len(), 1);
    let bmo = provider::enabled_by_key(&providers, "bmo:bugzilla.mozilla.org").unwrap();
    assert_eq!(bmo.kind, ProviderKind::Bmo);
    assert_eq!(bmo.oauth_client_id.as_deref(), Some("client"));
    assert!(!format!("{bmo:?}").contains("s3cret"));

    let mut account = external_account::for_identifiers(database, bmo, &["42".to_owned()])
        .await
        .unwrap()
        .unwrap();
    assert!(account.phid.is_none());
    account.profile = AccountProfile {
        username: Some("nick".to_owned()),
        real_name: None,
        email: Some("a@example.com".to_owned()),
    };
    account.set_property("oauth.token.access", json!("t/1"));
    external_account::save(database, &mut account, NOW)
        .await
        .unwrap();
    assert_eq!(account.account_secret().map(str::len), Some(32));
    assert_eq!(
        text(
            &test_db,
            "SELECT CONCAT_WS('|', accountType, accountDomain, accountID, username, \
               IFNULL(realName, '-'), email, emailVerified, properties, dateCreated) \
             FROM `{ns}_user`.user_externalaccount"
        )
        .await,
        format!(
            "bmo|bugzilla.mozilla.org||nick|-|a@example.com|0|{{\"oauth.token.access\":\"t\\/1\"}}|{NOW}"
        )
    );
    assert_eq!(
        text(
            &test_db,
            "SELECT CONCAT_WS('|', identifierHash, identifierRaw, providerConfigPHID) \
             FROM `{ns}_user`.user_externalaccountidentifier"
        )
        .await,
        format!("{}|42|PHID-AUTH-bmo", hash::digest_for_index(b"42"))
    );

    // Found again, and saved without a second identifier.
    let mut again = external_account::for_identifiers(database, bmo, &["42".to_owned()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.phid, account.phid);
    assert_eq!(again.property("oauth.token.access"), Some(&json!("t/1")));
    external_account::save(database, &mut again, NOW + 5)
        .await
        .unwrap();
    assert_eq!(
        text(
            &test_db,
            "SELECT CONCAT_WS('|', COUNT(*), MAX(dateModified)) \
             FROM `{ns}_user`.user_externalaccountidentifier"
        )
        .await,
        format!("1|{}", NOW + 5)
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn passwords_and_rate_limits() {
    let Some(test_db) = setup().await else {
        return;
    };
    let database = test_db.database();
    let admin = Phid::new("PHID-USER-admin");
    assert_eq!(
        password::is_valid_account_password(database, &admin, &Password::new("admin-password"))
            .await
            .unwrap(),
        Outcome::Done(true)
    );
    assert_eq!(
        password::is_valid_account_password(database, &admin, &Password::new("Admin-password"))
            .await
            .unwrap(),
        Outcome::Done(false)
    );
    assert_eq!(
        password::is_valid_account_password(
            database,
            &Phid::new("PHID-USER-nobody"),
            &Password::new("")
        )
        .await
        .unwrap(),
        Outcome::Done(false)
    );
    // Hashes PHP would upgrade on a match stay with PHP.
    test_db
        .execute("UPDATE `{ns}_auth`.auth_password SET legacyDigestFormat = 'v1' WHERE id = 1")
        .await;
    assert!(matches!(
        password::is_valid_account_password(database, &admin, &Password::new("x"))
            .await
            .unwrap(),
        Outcome::Unsupported(_)
    ));

    let actor = "10.0.0.1";
    assert!(
        !rate_limit::is_blocked(database, actor, rate_limit::TRY_PASSWORD, NOW)
            .await
            .unwrap()
    );
    rate_limit::record(database, actor, rate_limit::TRY_PASSWORD, 100.0, NOW)
        .await
        .unwrap();
    // `$score > $threshold`: 100 attempts an hour is still allowed.
    assert!(
        !rate_limit::is_blocked(database, actor, rate_limit::TRY_PASSWORD, NOW)
            .await
            .unwrap()
    );
    rate_limit::record(database, actor, rate_limit::TRY_PASSWORD, 1.0, NOW)
        .await
        .unwrap();
    assert!(
        rate_limit::is_blocked(database, actor, rate_limit::TRY_PASSWORD, NOW)
            .await
            .unwrap()
    );
    // Older than an hour, attempts don't count.
    assert!(
        !rate_limit::is_blocked(database, actor, rate_limit::TRY_PASSWORD, NOW + 3601)
            .await
            .unwrap()
    );
    assert!(
        !rate_limit::is_blocked(database, "10.0.0.2", rate_limit::TRY_PASSWORD, NOW)
            .await
            .unwrap()
    );
    test_db.drop_databases().await;
}
