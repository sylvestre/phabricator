//! The web routes against a test database.

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::{FALLBACK_HEADER, WebState};
use serde_json::json;
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

struct State {
    database: Database,
    config: Config,
    remarkup: phab_remarkup::Engine,
}

impl WebState for State {
    fn database(&self) -> &Database {
        &self.database
    }

    fn config(&self) -> &Config {
        &self.config
    }

    fn now(&self) -> i64 {
        NOW
    }

    fn remarkup(&self) -> &phab_remarkup::Engine {
        &self.remarkup
    }
}

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap()
}

/// Users `alice` (fine), `gone` (disabled) and `mfa` (partial session),
/// with sessions `alice-token`, `gone-token` and `mfa-token`.
async fn setup(with_csrf_key: bool) -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "user",
        "auth",
        "differential",
        "project",
        "policy",
        "spaces",
        "repository",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice <A>', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-gone', 'gone', 'Gone', 1, 1, 'c', 0, 1, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-mfa', 'mfa', 'Mfa', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 1, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0), \
             ('PHID-USER-gone', 'web', '{gone}', 1, {expires}, 'PHID-SSSN-2', 0), \
             ('PHID-USER-mfa', 'web', '{mfa}', 1, {expires}, 'PHID-SSSN-3', 1)",
            alice = session_key("alice-token"),
            gone = session_key("gone-token"),
            mfa = session_key("mfa-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    if with_csrf_key {
        test_db
            .execute(
                "INSERT INTO `{ns}_auth`.auth_hmackey (keyName, keyValue, dateCreated, \
                 dateModified) VALUES ('csrf', '6b3379', 1, 1)",
            )
            .await;
    }
    Some(test_db)
}

async fn get(test_db: &TestDatabase, config: Config, path: &str, cookie: Option<&str>) -> Response {
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn text(response: Response) -> String {
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn fallback_reason(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    response.headers()[FALLBACK_HEADER].to_str().unwrap()
}

#[tokio::test]
async fn logged_in_users_see_the_home_page() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    let mut config = Config::default();
    config.push_layer(
        "test",
        serde_json::Map::from_iter([
            ("moz-motd.title".to_owned(), json!("Maintenance <soon>")),
            (
                "moz-motd.message".to_owned(),
                json!("Read <a href=\"/w/\">this</a>."),
            ),
        ]),
    );
    let response = get(
        &test_db,
        config,
        "/",
        Some("phsid=alice-token; phab_ui=rust"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "Deny");
    let page = text(response).await;
    assert!(page.contains("<summary>alice</summary>"), "{page}");
    assert!(page.contains(r#"<meta name="csrf-token" content="B@"#));
    // The title is escaped, the message is trusted HTML (hsprintf()).
    assert!(page.contains("Maintenance &#60;soon&#62;"), "{page}");
    assert!(page.contains(r#"Read <a href="/w/">this</a>."#));
    assert!(page.contains(r#"href="/rust-ui/off?next=%2F""#));
    test_db.drop_databases().await;
}

#[tokio::test]
async fn other_viewers_go_to_php() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    for (cookie, reason) in [
        (None, "logged out"),
        (Some("phsid="), "logged out"),
        (Some("phsid=unknown-token"), "logged out"),
        (Some("phsid=A/anonymous"), "logged out"),
        (Some("phsid=gone-token"), "the account is not activated"),
        (
            Some("phsid=mfa-token"),
            "multi-factor authentication is pending",
        ),
    ] {
        let response = get(&test_db, Config::default(), "/", cookie).await;
        assert_eq!(fallback_reason(&response), reason, "{cookie:?}");
    }
    test_db.drop_databases().await;
}

#[tokio::test]
async fn missing_csrf_key_goes_to_php() {
    let Some(test_db) = setup(false).await else {
        return;
    };
    let response = get(&test_db, Config::default(), "/", Some("phsid=alice-token")).await;
    assert_eq!(
        fallback_reason(&response),
        "the CSRF key does not exist yet"
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn profiles_show_what_the_viewer_may_see() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user_profile (userPHID, title, blurb, icon, dateCreated, dateModified) \
             VALUES ('PHID-USER-gone', '', 'Works on **D1**.', 'engineering', 1, 1); \
             INSERT INTO `{ns}_project`.project (name, phid, authorPHID, dateCreated, dateModified, status, \
               viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, hasWorkboard, hasMilestones, \
               hasSubprojects, projectPath, projectDepth, projectPathKey, properties, subtype, primarySlug) \
             VALUES \
             ('Build', 'PHID-PROJ-build', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', 'blue', \
               'k', 0, 0, 0, 'a', 0, 'k001', '{}', 'default', 'build'), \
             ('Hidden', 'PHID-PROJ-hidden', 'x', 1, 1, '0', 'no-one', 'users', 'users', 'fa-x', 'blue', \
               'k', 0, 0, 0, 'b', 0, 'k002', '{}', 'default', 'hidden'); \
             INSERT INTO `{ns}_user`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-USER-gone', 14, 'PHID-PROJ-build', 1, 0), \
               ('PHID-USER-gone', 14, 'PHID-PROJ-hidden', 1, 0); \
             INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'One', 'PHID-DREV-one', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
               'users', 'users', '{}', ''); \
             INSERT INTO `{ns}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
               reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-one', 'PHID-USER-gone', 'added', 1, 1, '{}')",
        )
        .await;

    let response = get(
        &test_db,
        Config::default(),
        "/p/GONE/",
        Some("phsid=alice-token"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("<title>gone · Phabricator</title>"), "{page}");
    assert!(
        page.contains("Gone <span class=\"muted\">(gone)</span>"),
        "{page}"
    );
    assert!(page.contains("fa-code"), "{page}");
    assert!(page.contains("<dd>Disabled</dd>"), "{page}");
    assert!(page.contains(">1</a></dd>"), "review queue: {page}");
    assert!(page.contains("Works on <strong><a href=\"/D1\""), "{page}");
    assert!(page.contains("<a href=\"/tag/build/\">Build</a>"), "{page}");
    assert!(!page.contains("Hidden"), "{page}");

    let response = get(
        &test_db,
        Config::default(),
        "/p/nobody/",
        Some("phsid=alice-token"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn stylesheet_is_served() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    let uri = phab_web::assets::uri("phab.css");
    let response = get(&test_db, Config::default(), &uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/css; charset=UTF-8"
    );
    test_db.drop_databases().await;
}
