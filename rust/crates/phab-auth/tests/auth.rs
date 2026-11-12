use phab_auth::session::SessionType;
use phab_auth::token::InvalidAuth;
use phab_auth::{Outcome, Writes, session, token};
use phab_core::{Config, hash};
use phab_db::testing::TestDatabase;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&["user", "auth", "conduit"]).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0), \
             ('PHID-USER-gone', 'gone', 'Gone', 1, 1, 'c', 0, 1, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bot', 'bot', 'Bot', 1, 1, 'c', 1, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-alice', 'api', 'api-aaaaaaaaaaaaaaaaaaaaaaaaaaaa', NULL, 1, 1), \
             ('PHID-USER-alice', 'cli', 'cli-aaaaaaaaaaaaaaaaaaaaaaaaaaaa', {later}, 1, 1), \
             ('PHID-USER-alice', 'api', 'api-expiredexpiredexpiredexpiree', {earlier}, 1, 1), \
             ('PHID-USER-gone', 'api', 'api-gonegonegonegonegonegonegone', NULL, 1, 1), \
             ('PHID-USER-nobody', 'api', 'api-orphanorphanorphanorphanorpo', NULL, 1, 1), \
             ('PHID-USER-alice', 'clr', 'clr-aaaaaaaaaaaaaaaaaaaaaaaaaaaa', NULL, 1, 1), \
             ('PHID-DVCE-device', 'api', 'api-deviceddeviceddeviceddeviced', NULL, 1, 1)",
            later = NOW + 3600,
            earlier = NOW - 1,
        ))
        .await;
    Some(test_db)
}

fn invalid(message: &str) -> Outcome<Result<String, InvalidAuth>> {
    Outcome::Done(Err(InvalidAuth(message.to_owned())))
}

async fn authenticate(test_db: &TestDatabase, token: &str) -> Outcome<Result<String, InvalidAuth>> {
    let outcome = token::authenticate_token(
        test_db.database(),
        &Config::default(),
        token,
        NOW,
        Writes::Allowed,
    )
    .await
    .unwrap();
    match outcome {
        Outcome::Done(result) => Outcome::Done(result.map(|user| user.user_name)),
        Outcome::Unsupported(reason) => Outcome::Unsupported(reason),
    }
}

#[tokio::test]
async fn api_tokens() {
    let Some(test_db) = setup().await else {
        return;
    };

    assert_eq!(
        authenticate(&test_db, "api-aaaaaaaaaaaaaaaaaaaaaaaaaaaa").await,
        Outcome::Done(Ok("alice".to_owned()))
    );
    assert_eq!(
        authenticate(&test_db, "api-expiredexpiredexpiredexpiree").await,
        invalid(
            "API token \"api-expiredexpiredexpiredexpiree\" was previously valid, but has expired."
        )
    );
    assert_eq!(
        authenticate(&test_db, "api-unknownunknownunknownunknown").await,
        invalid("API token \"api-unknownunknownunknownunknown\" is not valid.")
    );
    assert_eq!(
        authenticate(&test_db, "api-orphanorphanorphanorphanorpo").await,
        invalid("API token \"api-orphanorphanorphanorphanorpo\" is not valid.")
    );
    assert_eq!(
        authenticate(&test_db, "api-gonegonegonegonegonegonegone").await,
        invalid("User account is not permitted to use the API.")
    );
    assert_eq!(
        authenticate(&test_db, "clr-aaaaaaaaaaaaaaaaaaaaaaaaaaaa").await,
        invalid(
            "This request originates from outside of the cluster address range. Requests \
             signed with cluster API tokens must originate from within the cluster."
        )
    );
    assert!(matches!(
        authenticate(&test_db, "api-deviceddeviceddeviceddeviced").await,
        Outcome::Unsupported(_)
    ));

    // Using a "cli-" token makes it permanent, unless writes are skipped.
    let expires = || async {
        phab_db::sqlx::query_scalar::<_, Option<u32>>(&format!(
            "SELECT expires FROM {} WHERE tokenType = 'cli'",
            test_db.database().table("conduit", "conduit_token")
        ))
        .fetch_one(test_db.database().pool())
        .await
        .unwrap()
    };
    let shadow = token::authenticate_token(
        test_db.database(),
        &Config::default(),
        "cli-aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        NOW,
        Writes::Skipped,
    )
    .await
    .unwrap();
    assert!(matches!(shadow, Outcome::Done(Ok(_))));
    assert!(expires().await.is_some());
    assert_eq!(
        authenticate(&test_db, "cli-aaaaaaaaaaaaaaaaaaaaaaaaaaaa").await,
        Outcome::Done(Ok("alice".to_owned()))
    );
    assert_eq!(expires().await, None);

    test_db.drop_databases().await;
}

#[tokio::test]
async fn sessions() {
    let Some(test_db) = setup().await else {
        return;
    };
    let config = Config::default();
    let database = test_db.database();

    let missing_key = session::load_user_for_session(
        database,
        &config,
        SessionType::Web,
        "token",
        NOW,
        Writes::Allowed,
    )
    .await
    .unwrap();
    assert!(matches!(missing_key, Outcome::Unsupported(_)));

    let digest_key = hex_decode(DIGEST_KEY_HEX);
    let strong = hash::hmac_sha256(b"strongtoken", &digest_key).unwrap();
    let weak = hash::weak_digest(b"weaktoken", config.hmac_key().unwrap().as_bytes()).unwrap();
    let bot = hash::hmac_sha256(b"bottoken", &digest_key).unwrap();
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid) VALUES \
             ('PHID-USER-alice', 'web', '{strong}', 1, {fresh}, 'PHID-SSSN-1'), \
             ('PHID-USER-alice', 'web', '{weak}', 1, {stale}, 'PHID-SSSN-2'), \
             ('PHID-USER-bot', 'web', '{bot}', 1, {fresh}, 'PHID-SSSN-3')",
            fresh = NOW + 29 * 86_400,
            stale = NOW + 86_400,
        ))
        .await;

    let config = &config;
    let lookup = |token: &'static str, session_type: SessionType, writes: Writes| async move {
        match session::load_user_for_session(database, config, session_type, token, NOW, writes)
            .await
            .unwrap()
        {
            Outcome::Done(found) => found.map(|(user, session)| (user.user_name, session.phid)),
            Outcome::Unsupported(reason) => panic!("unsupported: {reason}"),
        }
    };

    let found = lookup("strongtoken", SessionType::Web, Writes::Allowed).await;
    assert_eq!(found.map(|(name, _)| name).as_deref(), Some("alice"));
    assert_eq!(
        lookup("strongtoken", SessionType::Conduit, Writes::Allowed).await,
        None
    );
    assert_eq!(
        lookup("A/strongtoken", SessionType::Web, Writes::Allowed).await,
        None
    );
    assert_eq!(
        lookup("bottoken", SessionType::Web, Writes::Allowed).await,
        None
    );
    assert_eq!(
        lookup("nope", SessionType::Web, Writes::Allowed).await,
        None
    );

    let session_row = || async {
        phab_db::sqlx::query_as::<_, (String, u32)>(&format!(
            "SELECT CAST(sessionKey AS CHAR), sessionExpires FROM {} WHERE phid = 'PHID-SSSN-2'",
            database.table("user", "phabricator_session")
        ))
        .fetch_one(database.pool())
        .await
        .unwrap()
    };

    // A legacy (weak) key is accepted; writes upgrade it and extend the
    // nearly expired session.
    assert!(
        lookup("weaktoken", SessionType::Web, Writes::Skipped)
            .await
            .is_some()
    );
    assert_eq!(session_row().await.0, weak);
    assert!(
        lookup("weaktoken", SessionType::Web, Writes::Allowed)
            .await
            .is_some()
    );
    let (key, expires) = session_row().await;
    assert_eq!(key, hash::hmac_sha256(b"weaktoken", &digest_key).unwrap());
    assert!(i64::from(expires) > NOW + 2 * 86_400);

    test_db.drop_databases().await;
}

fn hex_decode(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}
