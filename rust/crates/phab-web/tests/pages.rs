//! The web routes against a test database.

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::searchfox::Trees;
use phab_web::{FALLBACK_HEADER, WebState};
use serde_json::json;
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

struct State {
    database: Database,
    config: Config,
    remarkup: phab_remarkup::Engine,
    php: Option<phab_web::php::PhpConduit>,
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

    fn php(&self) -> Option<&phab_web::php::PhpConduit> {
        self.php.as_ref()
    }

    fn searchfox_trees(&self) -> &Trees {
        static TREES: std::sync::OnceLock<Trees> = std::sync::OnceLock::new();
        TREES.get_or_init(|| Trees::parse("mozilla-central=mozilla-central"))
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
        "feed",
        "differential",
        "project",
        "policy",
        "spaces",
        "repository",
        "file",
        "harbormaster",
        "search",
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
    send(
        test_db,
        config,
        None,
        Request::get(path),
        cookie,
        Body::empty(),
    )
    .await
}

async fn send(
    test_db: &TestDatabase,
    config: Config,
    php: Option<phab_web::php::PhpConduit>,
    mut request: axum::http::request::Builder,
    cookie: Option<&str>,
    body: Body,
) -> Response {
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        php,
        config,
    });
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

/// What the fake PHP received: the cookie, the skip header and the form.
type Received = Arc<std::sync::Mutex<Vec<(String, String, String, String)>>>;

/// A stand-in for PHP's Conduit API, answering `answer`.
async fn fake_php(answer: &'static str) -> (phab_web::php::PhpConduit, Received) {
    let received: Received = Arc::default();
    let log = received.clone();
    let form_log = received.clone();
    // The comment form: saved (a redirect), or PHP's dialog with the error
    // of `answer`.
    let comment_form = move |axum::extract::Path(id): axum::extract::Path<String>,
                             headers: axum::http::HeaderMap,
                             body: String| {
        let log = form_log.clone();
        async move {
            let header = |name: &str| {
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_owned()
            };
            log.lock().unwrap().push((
                "comment form".to_owned(),
                header("cookie"),
                header("x-phab-skip-rust"),
                body,
            ));
            let error = serde_json::from_str::<serde_json::Value>(answer)
                .ok()
                .and_then(|envelope| envelope["error_info"].as_str().map(str::to_owned));
            match error {
                Some(error) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/html; charset=UTF-8".to_owned())],
                    format!("<div class=\"dialog\">{error}</div>"),
                ),
                None => (
                    StatusCode::FOUND,
                    [(header::LOCATION, format!("http://phabricator.test/D{id}"))],
                    String::new(),
                ),
            }
        }
    };
    let app = axum::Router::new()
        .route(
            "/differential/revision/edit/{id}/comment/",
            axum::routing::post(comment_form),
        )
        .route(
            "/api/{method}",
            axum::routing::post(
                move |axum::extract::Path(method): axum::extract::Path<String>,
                      headers: axum::http::HeaderMap,
                      body: String| {
                    let log = log.clone();
                    async move {
                        let header = |name: &str| {
                            headers
                                .get(name)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("")
                                .to_owned()
                        };
                        log.lock().unwrap().push((
                            method,
                            header("cookie"),
                            header("x-phab-skip-rust"),
                            body,
                        ));
                        ([(header::CONTENT_TYPE, "application/json")], answer)
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        phab_web::php::PhpConduit::new(format!("http://{address}")),
        received,
    )
}

/// The CSRF token of a page, as the browser would post it.
fn csrf_token(page: &str) -> String {
    page.split("name=\"csrf-token\" content=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned()
}

fn form_value(body: &str, name: &str) -> String {
    form_urlencoded::parse(body.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
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
    assert!(
        page.contains("<span class=\"fa fa-home\" aria-hidden=\"true\"></span> Home</h1>"),
        "{page}"
    );
    assert!(page.contains("<h2>Recent Activity</h2>"), "{page}");
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
async fn accounts_which_cannot_be_used_yet_say_why() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    let response = get(&test_db, Config::default(), "/", Some("phsid=gone-token")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let page = text(response).await;
    assert!(page.contains("<h1>Account Disabled</h1>"), "{page}");
    assert!(page.contains(r#"<a href="/logout/">Okay</a>"#), "{page}");

    // Waiting for approval, with the instructions an administrator wrote.
    test_db
        .execute(
            "UPDATE `{ns}_user`.user SET isApproved = 0 WHERE phid = 'PHID-USER-alice'; \
             INSERT INTO `{ns}_auth`.auth_message (phid, messageKey, messageText, dateCreated, \
               dateModified) VALUES ('PHID-AMSG-1', 'auth.wait-for-approval', \
               'Ask in **#phabricator**.', 1, 1)",
        )
        .await;
    let response = get(&test_db, Config::default(), "/", Some("phsid=alice-token")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let page = text(response).await;
    assert!(page.contains("<h1>Wait for Approval</h1>"), "{page}");
    assert!(
        page.contains("Ask in <strong>#phabricator</strong>."),
        "{page}"
    );

    // Verifying an email address still needs PHP, which sends the email.
    test_db
        .execute(
            "UPDATE `{ns}_user`.user SET isApproved = 1, isEmailVerified = 0 \
             WHERE phid = 'PHID-USER-alice'",
        )
        .await;
    let verify = config_with(&[("auth.require-email-verification", json!(true))]);
    let response = get(&test_db, verify, "/", Some("phsid=alice-token")).await;
    assert_eq!(
        fallback_reason(&response),
        "the email address is not verified"
    );
    test_db.drop_databases().await;
}

fn config_with(entries: &[(&str, serde_json::Value)]) -> Config {
    let mut config = Config::default();
    config.push_layer(
        "test",
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    );
    config
}

#[tokio::test]
async fn pages_need_their_application() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug, parentProjectPHID, \
               milestoneNumber) VALUES \
             (1, 'Build', 'PHID-PROJ-build', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-cog', \
               'blue', 'k', 0, 0, 0, 'a', 0, 'k001', '{}', 'default', 'build', NULL, NULL); \
             INSERT INTO `{ns}_project`.project_slug (projectPHID, slug, dateCreated, \
               dateModified) VALUES ('PHID-PROJ-build', 'build', 1, 1); \
             INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'One', 'PHID-DREV-one', 'published', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
               'users', 'users', '{}', '')",
        )
        .await;
    let alice = Some("phsid=alice-token");
    let paths = [
        "/",
        "/differential/",
        "/D1",
        "/tag/build/",
        "/project/view/1/",
    ];
    for path in paths {
        let response = get(&test_db, Config::default(), path, alice).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    let uninstalled = config_with(&[(
        "phabricator.uninstalled-applications",
        json!({
            "PhabricatorHomeApplication": true,
            "PhabricatorDifferentialApplication": true,
            "PhabricatorProjectApplication": true,
        }),
    )]);
    for path in paths {
        let response = get(&test_db, uninstalled.clone(), path, alice).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let response = send(
        &test_db,
        uninstalled,
        None,
        Request::post("/D1/action")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        alice,
        Body::from("__csrf__=x&action=comment"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Installed, but only administrators may use it.
    let admins_only = config_with(&[(
        "phabricator.application-settings",
        json!({"PHID-APPS-PhabricatorDifferentialApplication": {"policy": {"view": "admin"}}}),
    )]);
    let response = get(&test_db, admins_only.clone(), "/differential/", alice).await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    // People and Feed can't be uninstalled or restricted.
    let response = get(&test_db, admins_only, "/p/alice/", alice).await;
    assert_eq!(response.status(), StatusCode::OK);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn multi_factor_enrollment_is_required_when_configured() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    let alice = Some("phsid=alice-token");
    let required = config_with(&[("security.require-multi-factor-auth", json!(true))]);
    let response = get(&test_db, required.clone(), "/feed/", alice).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let page = text(response).await;
    assert!(page.contains("<h1>Add Multi-Factor Auth</h1>"), "{page}");
    assert!(page.contains(r#"href="/auth/enroll/""#), "{page}");

    // A factor of a disabled provider doesn't count.
    test_db
        .execute(
            "INSERT INTO `{ns}_auth`.auth_factorprovider (phid, providerFactorKey, status, \
               properties, dateCreated, dateModified, name) VALUES \
             ('PHID-FPRV-off', 'totp', 'disabled', '{}', 1, 1, ''), \
             ('PHID-FPRV-on', 'totp', 'active', '{}', 1, 1, ''); \
             INSERT INTO `{ns}_auth`.auth_factorconfig (phid, userPHID, factorName, \
               factorSecret, properties, dateCreated, dateModified, factorProviderPHID) VALUES \
             ('PHID-AFTR-1', 'PHID-USER-alice', '', '', '{}', 1, 1, 'PHID-FPRV-off')",
        )
        .await;
    let response = get(&test_db, required.clone(), "/feed/", alice).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    test_db
        .execute(
            "INSERT INTO `{ns}_auth`.auth_factorconfig (phid, userPHID, factorName, \
               factorSecret, properties, dateCreated, dateModified, factorProviderPHID) VALUES \
             ('PHID-AFTR-2', 'PHID-USER-alice', '', '', '{}', 1, 1, 'PHID-FPRV-on')",
        )
        .await;
    let response = get(&test_db, required, "/feed/", alice).await;
    assert_eq!(response.status(), StatusCode::OK);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn viewers_need_a_space_when_there_are_spaces() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    let alice = Some("phsid=alice-token");
    test_db
        .execute(
            "INSERT INTO `{ns}_spaces`.spaces_namespace (phid, namespaceName, viewPolicy, \
               editPolicy, isDefaultNamespace, dateCreated, dateModified, description, \
               isArchived) VALUES \
             ('PHID-SPCE-staff', 'Staff', 'no-one', 'no-one', 1, 1, 1, '', 0)",
        )
        .await;
    let response = get(&test_db, Config::default(), "/feed/", alice).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let page = text(response).await;
    assert!(page.contains("<h1>No Access to Spaces</h1>"), "{page}");
    // Going home would only lead here again.
    assert!(!page.contains("Return home"), "{page}");

    test_db
        .execute(
            "INSERT INTO `{ns}_spaces`.spaces_namespace (phid, namespaceName, viewPolicy, \
               editPolicy, isDefaultNamespace, dateCreated, dateModified, description, \
               isArchived) VALUES \
             ('PHID-SPCE-all', 'Everyone', 'users', 'users', NULL, 1, 1, '', 0)",
        )
        .await;
    let response = get(&test_db, Config::default(), "/feed/", alice).await;
    assert_eq!(response.status(), StatusCode::OK);
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

    // The revisions of alice (user 1), and of gone (user 2), who has none.
    assert!(page.contains("href=\"/people/revisions/2/\""), "{page}");
    let response = get(
        &test_db,
        Config::default(),
        "/people/revisions/1/",
        Some("phsid=alice-token"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(
        page.contains("<title>Recent Revisions · alice · Phabricator</title>"),
        "{page}"
    );
    assert!(page.contains("<a href=\"/D1\">One</a>"), "{page}");
    assert!(page.contains("Needs Review"), "{page}");
    let response = get(
        &test_db,
        Config::default(),
        "/people/revisions/2/",
        Some("phsid=alice-token"),
    )
    .await;
    assert!(text(response).await.contains("No recent revisions."));
    let response = get(
        &test_db,
        Config::default(),
        "/people/revisions/99/",
        Some("phsid=alice-token"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn project_pages_follow_policy() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    let description_index = hash::digest_for_index(b"std:project:internal:description");
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, dateModified, \
               status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, hasWorkboard, \
               hasMilestones, hasSubprojects, projectPath, projectDepth, projectPathKey, properties, \
               subtype, primarySlug, parentProjectPHID, milestoneNumber) VALUES \
             (1, 'Build', 'PHID-PROJ-build', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-cog', 'blue', \
               'k', 0, 0, 0, 'a', 0, 'k001', '{{}}', 'default', 'build', NULL, NULL), \
             (2, 'Hidden', 'PHID-PROJ-hidden', 'x', 1, 1, '0', 'no-one', 'users', 'users', 'fa-x', 'red', \
               'k', 0, 0, 0, 'b', 0, 'k002', '{{}}', 'default', 'hidden', NULL, NULL), \
             (3, 'Sprint 1', 'PHID-PROJ-sprint', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', 'red', \
               'k', 0, 0, 0, 'c', 1, 'k003', '{{}}', 'default', NULL, 'PHID-PROJ-build', 1); \
             INSERT INTO `{{ns}}_project`.project_slug (projectPHID, slug, dateCreated, dateModified) VALUES \
               ('PHID-PROJ-build', 'build', 1, 1), ('PHID-PROJ-build', 'builds', 1, 1), \
               ('PHID-PROJ-hidden', 'hidden', 1, 1); \
             INSERT INTO `{{ns}}_project`.project_customfieldstorage (objectPHID, fieldIndex, fieldValue) \
               VALUES ('PHID-PROJ-build', '{description_index}', 'Builds **Firefox**.'); \
             INSERT INTO `{{ns}}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-PROJ-build', 13, 'PHID-USER-alice', 1, 0), \
               ('PHID-PROJ-build', 47, 'PHID-USER-mfa', 1, 0), \
               ('PHID-PROJ-build', 42, 'PHID-DREV-one', 1, 0), \
               ('PHID-PROJ-build', 42, 'PHID-DREV-secret', 1, 0); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'One', 'PHID-DREV-one', 'published', '', '', 'PHID-USER-alice', 1, 1, '{{}}', 'k', \
               'users', 'users', '{{}}', ''), \
             (2, 'Secret', 'PHID-DREV-secret', 'needs-review', '', '', 'PHID-USER-mfa', 1, 1, '{{}}', 'k', \
               'no-one', 'no-one', '{{}}', '')"
        ))
        .await;
    let alice = Some("phsid=alice-token");

    let response = get(&test_db, Config::default(), "/tag/build/", alice).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(
        page.contains("<title>Build · Phabricator</title>"),
        "{page}"
    );
    assert!(page.contains("Builds <strong>Firefox</strong>."), "{page}");
    assert!(page.contains("<a href=\"/p/alice/\">alice</a>"), "{page}");
    assert!(page.contains("1 watching"), "{page}");
    assert!(
        page.contains("<a href=\"/project/view/3/\">Build (Sprint 1)</a>"),
        "{page}"
    );
    assert!(
        page.contains("class=\"remarkup-object-closed\">D1: One</a>"),
        "{page}"
    );
    assert!(!page.contains("Secret"), "{page}");

    // Other hashtags, and other cases, go to the primary one.
    let response = get(&test_db, Config::default(), "/tag/Builds/", alice).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()[header::LOCATION], "/tag/build/");

    let response = get(&test_db, Config::default(), "/project/view/3/", alice).await;
    assert!(
        text(response)
            .await
            .contains("<title>Build (Sprint 1) · Phabricator</title>")
    );

    // A project the viewer can't see is denied, and not redirected.
    for path in ["/tag/hidden/", "/tag/HIDDEN/", "/project/view/2/"] {
        let response = get(&test_db, Config::default(), path, alice).await;
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    }
    for path in ["/tag/nope/", "/project/view/9/"] {
        let response = get(&test_db, Config::default(), path, alice).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    project_subpages_follow_policy(&test_db).await;
    test_db.drop_databases().await;
}

/// The members and subprojects pages of `project_pages_follow_policy()`'s
/// projects.
async fn project_subpages_follow_policy(test_db: &TestDatabase) {
    let alice = Some("phsid=alice-token");
    // Every member and watcher; alice is a member, and doesn't watch.
    let response = get(test_db, Config::default(), "/project/members/1/", alice).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(
        page.contains("<title>Build · Members and Watchers · Phabricator</title>"),
        "{page}"
    );
    let members = page.find("<h2>Members (1)</h2>").expect("members");
    let watchers = page.find("<h2>Watchers (1)</h2>").expect("watchers");
    let alice_link = page.find("<a href=\"/p/alice/\">alice</a>").expect("alice");
    let mfa_link = page.find("<a href=\"/p/mfa/\">mfa</a>").expect("mfa");
    assert!(members < alice_link && alice_link < watchers && watchers < mfa_link);
    assert!(page.contains("/project/update/1/leave/"), "{page}");
    assert!(page.contains("/project/watch/1/"), "{page}");

    // The milestone, and no subprojects.
    let response = get(test_db, Config::default(), "/project/subprojects/1/", alice).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("This project has no subprojects."), "{page}");
    assert!(
        page.contains("<a href=\"/project/view/3/\">Build (Sprint 1)</a>"),
        "{page}"
    );
    let response = get(test_db, Config::default(), "/project/subprojects/3/", alice).await;
    assert!(
        text(response)
            .await
            .contains("Milestone projects do not support subprojects or milestones.")
    );
    for (path, status) in [
        ("/project/members/2/", StatusCode::MISDIRECTED_REQUEST),
        ("/project/subprojects/2/", StatusCode::MISDIRECTED_REQUEST),
        ("/project/members/9/", StatusCode::NOT_FOUND),
    ] {
        let response = get(test_db, Config::default(), path, alice).await;
        assert_eq!(response.status(), status, "{path}");
    }
}

/// D3 with reviewers, a bug, a stack, a timeline and a diff; it is
/// secure, and D6, open and public, depends on it.
async fn insert_revision_fixtures(test_db: &TestDatabase) {
    let bug_index = hash::digest_for_index(b"differential:bugzilla-bug-id");
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, properties, activeDiffPHID) VALUES \
             (3, 'Fix <crash>', 'PHID-DREV-three', 'needs-review', 'Fixes **it**.', '', \
               'PHID-USER-alice', 1, 1, '{{}}', 'k', 'users', 'users', '{{}}', 'PHID-DIFF-seven'), \
             (4, 'Parent', 'PHID-DREV-four', 'published', '', '', 'PHID-USER-alice', 1, 1, '{{}}', 'k', \
               'users', 'users', '{{}}', ''), \
             (5, 'Secret parent', 'PHID-DREV-five', 'needs-review', '', '', 'PHID-USER-mfa', 1, 1, \
               '{{}}', 'k', 'no-one', 'no-one', '{{}}', ''); \
             INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
               reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-three', 'PHID-USER-mfa', 'blocking', 1, 1, '{{}}'), \
             ('PHID-DREV-three', 'PHID-USER-gone', 'accepted', 1, 1, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.differential_customfieldstorage (objectPHID, \
               fieldIndex, fieldValue) VALUES ('PHID-DREV-three', '{bug_index}', '1234567'); \
             INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-DREV-three', 5, 'PHID-DREV-four', 1, 0), \
               ('PHID-DREV-three', 5, 'PHID-DREV-five', 1, 0), \
               ('PHID-DREV-three', 6, 'PHID-DREV-six', 1, 0), \
               ('PHID-DREV-three', 41, 'PHID-PROJ-secure', 1, 0); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, properties, activeDiffPHID) VALUES \
             (6, 'Public child', 'PHID-DREV-six', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'users', '{{}}', ''); \
             INSERT INTO `{{ns}}_project`.project (name, phid, authorPHID, dateCreated, dateModified, \
               status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, hasWorkboard, \
               hasMilestones, hasSubprojects, projectPath, projectDepth, projectPathKey, properties, \
               subtype) VALUES \
             ('secure-revision', 'PHID-PROJ-secure', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', \
               'red', 'k', 0, 0, 0, 'a', 0, 'k001', '{{}}', 'default'); \
             INSERT INTO `{{ns}}_differential`.differential_transaction (phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, \
               oldValue, newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('PHID-XACT-DREV-1', 'PHID-USER-alice', 'PHID-DREV-three', 'public', 'PHID-USER-alice', \
               'PHID-XCMT-1', 1, 'core:comment', 'null', 'null', '{{}}', '{{}}', 10, 10), \
             ('PHID-XACT-DREV-2', 'PHID-USER-mfa', 'PHID-DREV-three', 'public', 'PHID-USER-mfa', \
               NULL, 0, 'differential.revision.accept', 'false', 'true', '{{}}', '{{}}', 20, 20); \
             INSERT INTO `{{ns}}_differential`.differential_transaction_comment (phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, isNewFile, lineNumber, lineLength, \
               hasReplies, attributes) VALUES \
             ('PHID-XCMT-1', 'PHID-XACT-DREV-1', 'PHID-USER-alice', 'public', 'PHID-USER-alice', 1, \
               'Looks **good**', '{{}}', 0, 10, 10, 0, 0, 0, 0, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.differential_transaction_comment (phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, isNewFile, lineNumber, lineLength, \
               hasReplies, attributes, changesetID, revisionPHID) VALUES \
             ('PHID-XCMT-2', 'PHID-XACT-DREV-3', 'PHID-USER-alice', 'public', 'PHID-USER-alice', 1, \
               'Why **2**?', '{{}}', 0, 30, 30, 1, 2, 0, 0, '{{}}', 10, 'PHID-DREV-three'), \
             ('PHID-XCMT-3', 'PHID-XACT-DREV-4', 'PHID-USER-mfa', 'no-one', 'PHID-USER-mfa', 1, \
               'Hidden inline', '{{}}', 0, 30, 30, 1, 2, 0, 0, '{{}}', 10, 'PHID-DREV-three'), \
             ('PHID-XCMT-4', NULL, 'PHID-USER-alice', 'public', 'PHID-USER-alice', 1, \
               'Draft inline', '{{}}', 0, 30, 30, 1, 2, 0, 0, '{{}}', 10, 'PHID-DREV-three'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, repositoryPHID, sourceControlBaseRevision, lintStatus, unitStatus, \
               lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (7, 'PHID-DIFF-seven', 3, 'PHID-USER-alice', NULL, 'abc', 0, 0, 3, 1, 1, 'users'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, phid, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, newProperties, \
               addLines, delLines, dateCreated, dateModified) VALUES \
             (10, 'PHID-DCHG-ten', 7, NULL, 'src/a.c', '[]', 2, 1, '{{}}', '[]', '[]', 1, 1, 1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_hunk (id, changesetID, oldOffset, \
               oldLen, newOffset, newLen, dataType, dataEncoding, dataFormat, data, dateCreated, \
               dateModified) VALUES \
             (21, 10, 1, 2, 1, 2, 'text', 'utf8', 'byte', ' int x;\\n-int y = 1;\\n+int y = 2;\\n', 1, 1)"
        ))
        .await;
}

#[tokio::test]
async fn revisions_show_their_header_text_and_diff() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    insert_revision_fixtures(&test_db).await;
    test_db
        .execute(
            "INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy, repositorySlug) VALUES \
             (1, 'PHID-REPO-mc', 'mozilla-central', 'MOZILLACENTRAL', 'hg', '{}', 1, 1, \
               'users', 'users', 'users', 'mozilla-central'); \
             UPDATE `{ns}_differential`.differential_revision \
               SET repositoryPHID = 'PHID-REPO-mc' WHERE id = 3",
        )
        .await;

    let response = get(
        &test_db,
        Config::default(),
        "/D3",
        Some("phsid=alice-token"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("<h1>D3: Fix &#60;crash&#62;</h1>"), "{page}");
    assert!(page.contains("Needs Review"), "{page}");
    assert!(
        page.contains("<a href=\"/p/mfa/\">mfa</a> <span class=\"tag\">Blocking</span>"),
        "{page}"
    );
    assert!(
        page.contains("show_bug.cgi?id=1234567\">1234567</a>"),
        "{page}"
    );
    assert!(
        page.contains("Depends on <a href=\"/D4\" class=\"remarkup-object-closed\">D4: Parent</a>"),
        "{page}"
    );
    assert!(!page.contains("Secret parent"), "{page}");
    assert!(page.contains("Fixes <strong>it</strong>."), "{page}");
    assert!(page.contains("src/a.c"), "{page}");
    assert!(
        page.contains(
            "<span class=\"hl-type\">int</span> y <span class=\"hl-operator\">=</span> \
             <span class=\"bright\"><span class=\"hl-number\">2</span></span>;"
        ),
        "{page}"
    );
    // The repository has a Searchfox tree: it and the file link there.
    assert!(
        page.contains(
            "<dt>Repository</dt>\n  <dd><a \
             href=\"https://searchfox.org/mozilla-central/source/\">mozilla-central</a></dd>"
        ),
        "{page}"
    );
    assert!(
        page.contains(
            "<a class=\"changeset-searchfox\" \
             href=\"https://searchfox.org/mozilla-central/source/src/a.c\">View in Searchfox</a>"
        ),
        "{page}"
    );
    // D3 is secure, and D6, open and public, depends on it.
    assert!(page.contains("This is a secure revision."), "{page}");
    assert!(
        page.contains("A public revision is depending on this secure revision."),
        "{page}"
    );
    // Inline comments: the published one Alice may see, not the one she
    // may not, nor a draft.
    assert!(page.contains("Why <strong>2</strong>?"), "{page}");
    assert!(!page.contains("Hidden inline"), "{page}");
    assert!(!page.contains("Draft inline"), "{page}");

    // D6 is public, so no warning there.
    let response = get(
        &test_db,
        Config::default(),
        "/D6",
        Some("phsid=alice-token"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!text(response).await.contains("secure revision"));

    for (path, status) in [
        ("/D5", StatusCode::MISDIRECTED_REQUEST),
        ("/D9", StatusCode::NOT_FOUND),
        ("/Dx", StatusCode::NOT_FOUND),
    ] {
        let response = get(&test_db, Config::default(), path, Some("phsid=alice-token")).await;
        assert_eq!(response.status(), status, "{path}");
    }
    test_db.drop_databases().await;
}

#[tokio::test]
async fn revisions_show_their_merge_conflict_status() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    insert_revision_fixtures(&test_db).await;
    let index = hash::digest_for_index(b"differential:merge-conflict-status");
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-g', 'Git', 'G', 'git', '{{}}', 1, 1, 'users', 'users', 'users'); \
             UPDATE `{{ns}}_differential`.differential_revision \
               SET repositoryPHID = 'PHID-REPO-g' WHERE id = 3; \
             INSERT INTO `{{ns}}_differential`.differential_customfieldstorage (objectPHID, \
               fieldIndex, fieldValue) VALUES ('PHID-DREV-three', '{index}', \
               '{{\"status\":\"conflict\",\"reason\":\"D3 does not apply.\",\
             \"reasonCode\":\"patch-does-not-apply\",\
             \"checkedAgainstBaseCommit\":\"abcdef0123456789\",\
             \"checkedAgainstDiffPHID\":\"PHID-DIFF-seven\",\"checkedAgainstDiffID\":7,\
             \"epoch\":{NOW}}}')"
        ))
        .await;
    let mut config = Config::default();
    if let serde_json::Value::Object(map) = json!({
        "merge.conflict.enabled": true,
        "merge.conflict.repository-phids": ["PHID-REPO-g"],
    }) {
        config.push_layer("test", map);
    }
    let page = |config: Config| {
        let test_db = &test_db;
        async move { get(test_db, config, "/D3", Some("phsid=alice-token")).await }
    };

    let shown = text(page(config.clone()).await).await;
    assert!(shown.contains("<dt>Merge Conflict Status</dt>"), "{shown}");
    for expected in [
        "Landing may fail: this does not merge cleanly into the target branch",
        "<div class=\"muted\">D3 does not apply.</div>",
        "<div>Last checked 2023-11-14",
        "<div class=\"muted\">Diff 7, based on abcdef012345</div>",
        "The stack no longer applies to the commit it records as its base. Rebasing the stack \
         and updating its revisions will fix this.",
    ] {
        assert!(shown.contains(expected), "{expected}: {shown}");
    }

    // Merged from a revision Alice can't see: the reason and the hint go.
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_differential`.differential_customfieldstorage SET fieldValue = \
               '{{\"status\":\"clean\",\"reason\":\"D5 landed.\",\
             \"reasonCode\":\"base-not-ancestor\",\
             \"checkedAgainstBaseRevisionPHID\":\"PHID-DREV-five\"}}' WHERE fieldIndex = '{index}'"
        ))
        .await;
    let redacted = text(page(config.clone()).await).await;
    assert!(
        redacted.contains("Landing merges cleanly into the target branch"),
        "{redacted}"
    );
    assert!(
        redacted.contains("lands on top of revisions you do not have permission to see"),
        "{redacted}"
    );
    assert!(!redacted.contains("D5 landed."), "{redacted}");
    assert!(!redacted.contains("What to do"), "{redacted}");
    assert!(!redacted.contains("Last checked"), "{redacted}");

    // Computed for another diff.
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_differential`.differential_customfieldstorage SET fieldValue = \
               '{{\"status\":\"unknown\",\"reasonCode\":\"internal-error\",\
             \"checkedAgainstDiffPHID\":\"PHID-DIFF-old\"}}' WHERE fieldIndex = '{index}'"
        ))
        .await;
    let stale = text(page(config.clone()).await).await;
    assert!(stale.contains("Recomputing for the latest diff"), "{stale}");
    assert!(!stale.contains("What to do"), "{stale}");

    // A value PHP would coerce goes to PHP.
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_differential`.differential_customfieldstorage SET fieldValue = \
               '{{\"status\":\"unknown\",\"reasonCode\":5}}' WHERE fieldIndex = '{index}'"
        ))
        .await;
    let response = page(config.clone()).await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    assert!(response.headers().contains_key(FALLBACK_HEADER));

    // Disabled, or no status: no row.
    let disabled = text(page(Config::default()).await).await;
    assert!(!disabled.contains("Merge Conflict Status"), "{disabled}");
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_differential`.differential_customfieldstorage SET fieldValue = '[]' \
             WHERE fieldIndex = '{index}'"
        ))
        .await;
    let empty = text(page(config).await).await;
    assert!(!empty.contains("Merge Conflict Status"), "{empty}");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn revision_pages_have_what_the_script_needs() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    insert_revision_fixtures(&test_db).await;
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-three', 21, 'PHID-USER-alice', 1, 0)",
        )
        .await;
    let page = text(
        get(
            &test_db,
            Config::default(),
            "/D3",
            Some("phsid=alice-token"),
        )
        .await,
    )
    .await;
    // What phab.js needs to place new inline comments and replies.
    assert!(
        page.contains(r#"<section class="diff-section" data-revision="D3" data-diff="7">"#),
        "{page}"
    );
    assert!(
        page.contains(r#"<section class="changeset" data-path="src/a.c">"#),
        "{page}"
    );
    assert!(
        page.contains(r#"<td class="n" data-side="new" data-line="2">2</td>"#),
        "{page}"
    );
    assert!(
        page.contains(r#"<div class="inline-comment" data-phid="PHID-XCMT-2">"#),
        "{page}"
    );
    assert!(
        page.contains(r#"<input type="hidden" name="inlines" value="">"#),
        "{page}"
    );
    // Subscribers, and the buttons removing them and the reviewers.
    assert!(
        page.contains(
            r#"<dt>Subscribers</dt>
  <dd>
    <ul class="reviewers">
      <li><a href="/p/alice/">alice</a>"#
        ),
        "{page}"
    );
    assert!(
        page.contains(r#"<input type="hidden" name="remove_subscriber" value="PHID-USER-alice">"#),
        "{page}"
    );
    assert!(
        page.contains(r#"<input type="hidden" name="remove_reviewer" value="PHID-USER-mfa">"#),
        "{page}"
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn the_queue_groups_revisions_by_required_action() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'Review me', 'PHID-DREV-one', 'needs-review', '', '', 'PHID-USER-gone', 1, 1, '{}', \
               'k', 'users', 'users', '{}', ''), \
             (2, 'Mine to land', 'PHID-DREV-two', 'accepted', '', '', 'PHID-USER-alice', 1, 2, '{}', \
               'k', 'users', 'users', '{}', ''), \
             (3, 'Hidden', 'PHID-DREV-three', 'needs-review', '', '', 'PHID-USER-gone', 1, 3, '{}', \
               'k', 'no-one', 'no-one', '{}', ''), \
             (4, 'For my team', 'PHID-DREV-four', 'needs-review', '', '', 'PHID-USER-gone', 1, 4, '{}', \
               'k', 'users', 'users', '{}', ''); \
             INSERT INTO `{ns}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
               reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-one', 'PHID-USER-alice', 'added', 1, 1, '{}'), \
             ('PHID-DREV-three', 'PHID-USER-alice', 'added', 1, 1, '{}'), \
             ('PHID-DREV-four', 'PHID-PROJ-team', 'blocking', 1, 1, '{}'); \
             INSERT INTO `{ns}_project`.project (name, phid, authorPHID, dateCreated, dateModified, \
               status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, hasWorkboard, \
               hasMilestones, hasSubprojects, projectPath, projectDepth, projectPathKey, properties, \
               subtype) VALUES \
             ('Team', 'PHID-PROJ-team', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', 'red', \
               'k', 0, 0, 0, 'a', 0, 'k001', '{}', 'default'); \
             INSERT INTO `{ns}_user`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-USER-alice', 14, 'PHID-PROJ-team', 1, 0)",
        )
        .await;
    let response = get(
        &test_db,
        Config::default(),
        "/differential/",
        Some("phsid=alice-token"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    let section = |name: &str| {
        let start = page
            .find(&format!("<h2>{name}"))
            .unwrap_or_else(|| panic!("no {name}: {page}"));
        let end = page[start..]
            .find("</section>")
            .map_or(page.len(), |end| start + end);
        page[start..end].to_owned()
    };
    assert!(section("Must Review").contains("For my team"), "{page}");
    assert!(section("Ready to Review").contains("Review me"), "{page}");
    assert!(section("Ready to Land").contains("Mine to land"), "{page}");
    assert!(!page.contains("Hidden"), "{page}");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn actions_go_to_php_as_the_user() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    let alice = Some("phsid=alice-token");
    // A token from a page, as the browser would post it.
    let page = text(get(&test_db, Config::default(), "/differential/", alice).await).await;
    let token = csrf_token(&page);
    let post = |body: String, php| {
        let request = Request::post("/D3/action")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        send(
            &test_db,
            Config::default(),
            php,
            request,
            alice,
            Body::from(body),
        )
    };
    let encode = |pairs: &[(&str, &str)]| {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    };

    let (php, received) =
        fake_php(r#"{"result":{"object":{"id":3}},"error_code":null,"error_info":null}"#).await;
    let response = post(
        encode(&[
            ("__csrf__", &token),
            ("action", "accept"),
            ("comment", "LGTM"),
        ]),
        Some(php),
    )
    .await;
    // Rust doesn't write it (there is no D3): PHP's comment form gets it,
    // as PHP's page would post it.
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/D3");
    let calls = received.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let (method, cookie, skip, body) = &calls[0];
    assert_eq!(method, "comment form");
    assert_eq!(cookie, "phsid=alice-token");
    assert_eq!(skip, "1");
    assert!(form_value(body, "__csrf__").starts_with("B@"));
    assert_eq!(form_value(body, "__form__"), "1");
    assert_eq!(form_value(body, "comment"), "LGTM");
    assert_eq!(form_value(body, "comment_metadata"), "{}");
    let actions: serde_json::Value =
        serde_json::from_str(&form_value(body, "editengine.actions")).unwrap();
    assert_eq!(
        actions,
        json!([{"type": "accept", "value": true, "initialValue": null}])
    );

    // A stale token is refused before PHP hears about it.
    let (php, received) = fake_php("{}").await;
    let response = post(
        encode(&[("__csrf__", "B@nope"), ("action", "accept")]),
        Some(php),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(received.lock().unwrap().is_empty());

    // PHP's dialog is shown to the user.
    let (php, _) = fake_php(r#"{"result":null,"error_code":"ERR-CONDUIT-CORE","error_info":"You can not accept this revision."}"#).await;
    let response = post(
        encode(&[("__csrf__", &token), ("action", "accept")]),
        Some(php),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        text(response)
            .await
            .contains("You can not accept this revision.")
    );

    // PHP down: Rust's own error page, with a status nginx doesn't replay.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let php = phab_web::php::PhpConduit::new(format!("http://{address}"));
    let response = post(
        encode(&[("__csrf__", &token), ("action", "accept")]),
        Some(php),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(text(response).await.contains("could not be reached"));
    test_db.drop_databases().await;
}

/// Posts the action form of D3 as Alice to a PHP which accepts the edit:
/// the answer, and the transactions PHP got.
async fn post_edit(
    test_db: &TestDatabase,
    pairs: &[(&str, &str)],
) -> (Response, serde_json::Value) {
    let (php, received) =
        fake_php(r#"{"result":{"object":{"id":3}},"error_code":null,"error_info":null}"#).await;
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    let response = send(
        test_db,
        Config::default(),
        Some(php),
        Request::post("/D3/action")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Some("phsid=alice-token"),
        Body::from(body),
    )
    .await;
    let calls = received.lock().unwrap().clone();
    let transactions = calls.first().map_or(serde_json::Value::Null, |call| {
        if call.0 == "comment form" {
            serde_json::from_str(&form_value(&call.3, "editengine.actions")).unwrap()
        } else {
            serde_json::from_str::<serde_json::Value>(&form_value(&call.3, "params")).unwrap()
                ["transactions"]
                .clone()
        }
    });
    (response, transactions)
}

/// D3, Alice's draft which MFA reviews: Rust leaves its comment form to
/// PHP.
async fn insert_draft(test_db: &TestDatabase) {
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, properties, activeDiffPHID) VALUES \
             (3, 'Draft', 'PHID-DREV-three', 'draft', '', '', 'PHID-USER-alice', 1, 1, '{}', \
               'k', 'users', 'users', '{}', ''); \
             INSERT INTO `{ns}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
               reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-three', 'PHID-USER-mfa', 'added', 1, 1, '{}')",
        )
        .await;
}

#[tokio::test]
async fn reviewers_subscribers_and_inlines_go_to_php() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    let alice = Some("phsid=alice-token");
    let page = text(get(&test_db, Config::default(), "/differential/", alice).await).await;
    let token = csrf_token(&page);
    let post = |body: String, php| {
        let request = Request::post("/D3/action")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        send(
            &test_db,
            Config::default(),
            php,
            request,
            alice,
            Body::from(body),
        )
    };
    let encode = |pairs: &[(&str, &str)]| {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    };

    insert_draft(&test_db).await;

    // Names as PHIDs, `!` is blocking, in the lists PHP's tokenizers make.
    let pairs = [
        ("__csrf__", token.as_str()),
        ("reviewers", "MFA!"),
        ("subscribers", "@alice"),
    ];
    let (response, transactions) = post_edit(&test_db, &pairs).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        transactions,
        json!([
            {"type": "reviewers", "value": ["blocking(PHID-USER-mfa)"],
                "initialValue": ["PHID-USER-mfa"]},
            {"type": "subscriberPHIDs", "value": ["PHID-USER-alice"], "initialValue": []},
        ])
    );

    // Inline comments written on the page are published in the same edit.
    let inlines = r#"[{"path": "src/a.c", "line": 2, "isNewFile": true, "diff": 7, "content": "Why 2?"},
        {"replyToCommentPHID": "PHID-XCMT-2", "content": "Because."}]"#;
    let (response, transactions) = post_edit(
        &test_db,
        &[
            ("__csrf__", &token),
            ("inlines", inlines),
            ("comment", "See inline"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/D3#inlines-published"
    );
    assert_eq!(
        transactions,
        json!([
            {"type": "inline", "value": {"path": "src/a.c", "line": 2, "length": 1,
                "isNewFile": true, "diff": 7, "content": "Why 2?"}},
            {"type": "inline", "value": {"replyToCommentPHID": "PHID-XCMT-2", "content": "Because."}},
            {"type": "comment", "value": "See inline"},
        ])
    );
    let (php, received) = fake_php("{}").await;
    let response = post(
        encode(&[("__csrf__", &token), ("inlines", "[{\"path\": \"a\"}]")]),
        Some(php),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(received.lock().unwrap().is_empty());

    // The "×" next to a reviewer.
    let (response, transactions) = post_edit(
        &test_db,
        &[("__csrf__", &token), ("remove_reviewer", "PHID-USER-mfa")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        transactions,
        json!([{"type": "reviewers", "value": [], "initialValue": ["PHID-USER-mfa"]}])
    );

    // Unknown and disabled users are refused, saying who.
    for (name, message) in [
        ("nobody", "No user or project is named nobody."),
        ("gone", "No user or project is named gone."),
        ("#nope", "No user or project is named #nope."),
    ] {
        let (php, received) = fake_php("{}").await;
        let response = post(
            encode(&[("__csrf__", &token), ("reviewers", name)]),
            Some(php),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        assert!(text(response).await.contains(message), "{name}");
        assert!(received.lock().unwrap().is_empty());
    }

    test_db.drop_databases().await;
}

async fn suggest(test_db: &TestDatabase, query: &str) -> serde_json::Value {
    let response = get(
        test_db,
        Config::default(),
        &format!("/rust-ui/typeahead?q={query}"),
        Some("phsid=alice-token"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_str(&text(response).await).unwrap()
}

#[tokio::test]
async fn names_are_suggested_as_they_are_typed() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug) VALUES \
             (1, 'Mfa Team', 'PHID-PROJ-team', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', \
               'blue', 'k', 0, 0, 0, 'a', 0, 'k001', '{}', 'default', 'mfa-team'), \
             (2, 'Mfa Secret', 'PHID-PROJ-secret', 'x', 1, 1, '0', 'no-one', 'users', 'users', \
               'fa-x', 'blue', 'k', 0, 0, 0, 'b', 0, 'k002', '{}', 'default', 'mfa-secret'), \
             (3, 'Mfa Old', 'PHID-PROJ-old', 'x', 1, 1, '100', 'users', 'users', 'users', \
               'fa-x', 'blue', 'k', 0, 0, 0, 'c', 0, 'k003', '{}', 'default', 'mfa-old'); \
             INSERT INTO `{ns}_project`.project_slug (projectPHID, slug, dateCreated, \
               dateModified) VALUES ('PHID-PROJ-team', 'mfa-team', 1, 1), \
               ('PHID-PROJ-team', 'mfa_team', 1, 1), ('PHID-PROJ-secret', 'mfa-secret', 1, 1), \
               ('PHID-PROJ-old', 'mfa-old', 1, 1)",
        )
        .await;
    // Users by prefix, without case; the disabled one is left out.
    assert_eq!(
        suggest(&test_db, "%40M").await,
        json!([{"value": "mfa", "label": "mfa (Mfa)"}])
    );
    assert_eq!(suggest(&test_db, "g").await, json!([]));
    // `_` is not a wildcard.
    assert_eq!(suggest(&test_db, "a_").await, json!([]));
    // Projects: visible, not archived, once each.
    assert_eq!(
        suggest(&test_db, "%23MFA").await,
        json!([{"value": "#mfa-team", "label": "Mfa Team"}])
    );
    assert_eq!(suggest(&test_db, "%23").await, json!([]));
    test_db.drop_databases().await;
}

#[tokio::test]
async fn stylesheet_is_served() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    for (name, content_type) in [
        ("phab.css", "text/css; charset=UTF-8"),
        ("phab.js", "text/javascript; charset=UTF-8"),
    ] {
        let uri = phab_web::assets::uri(name);
        let response = get(&test_db, Config::default(), &uri, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
    }
    test_db.drop_databases().await;
}

#[tokio::test]
async fn comments_are_previewed() {
    let Some(test_db) = setup(true).await else {
        return;
    };
    let alice = Some("phsid=alice-token");
    let page = text(get(&test_db, Config::default(), "/differential/", alice).await).await;
    assert!(page.contains("/phab.js\" defer></script>"), "{page}");
    let token = csrf_token(&page);
    let preview = |body: String| {
        send(
            &test_db,
            Config::default(),
            None,
            Request::post("/rust-ui/preview")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            alice,
            Body::from(body),
        )
    };
    let form = |csrf: &str, text: &str| {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs([("__csrf__", csrf), ("text", text)])
            .finish()
    };

    let response = preview(form(&token, "Hi **@alice** <b>")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(
        html.contains(r#"<strong><a href="/p/alice/" class="remarkup-mention""#),
        "{html}"
    );
    assert!(html.contains("&lt;b&gt;"), "{html}");
    assert!(!html.contains("<html"), "{html}");

    let response = preview(form("B@stale", "Hi")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    test_db.drop_databases().await;
}
