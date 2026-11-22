//! The login pages against a test database and a fake Bugzilla.

// Each test walks a whole workflow.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Form, Json, Router, routing};
use phab_auth::csrf::{self, CsrfKey, CsrfSecret};
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::login::LoginSettings;
use phab_web::{FALLBACK_HEADER, WebState};
use serde_json::{Value, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
const CSRF_KEY_HEX: &str = "6b3379";
const ALTERNATE_KEY_HEX: &str = "a17e";
const HOST: &str = "phab.example";
/// Account secrets fill their `binary(64)` column.
const ACCOUNT_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const BMO_PROVIDER: &str = "PHID-AUTH-bmo";
const PASSWORD_PROVIDER: &str = "PHID-AUTH-password";
/// `password_hash(hash_hmac('sha256', 'admin-password', 'adminsalt'),
/// PASSWORD_BCRYPT, ['cost' => 11])`, made by PHP.
const ADMIN_HASH: &str = "bcrypt:$2y$11$R/PqsbNSjpoD3K9VMfjc3OU.dEM.OTSqYxHLKLPEPU.poSAZwFfj.";

struct State {
    database: Database,
    config: Config,
    remarkup: phab_remarkup::Engine,
    login: Option<LoginSettings>,
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
        None
    }

    fn login(&self) -> Option<&LoginSettings> {
        self.login.as_ref()
    }
}

/// A Bugzilla which knows the codes `code-<id>` (and `code-nomfa`), and
/// counts the token requests it gets.
struct FakeBugzilla {
    url: String,
    token_requests: Arc<AtomicUsize>,
}

async fn fake_bugzilla() -> FakeBugzilla {
    let token_requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&token_requests);
    let app = Router::new()
        .route(
            "/oauth/access_token",
            routing::post(move |Form(form): Form<Vec<(String, String)>>| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let field = |name: &str| {
                        form.iter()
                            .find(|(key, _)| key == name)
                            .map(|(_, value)| value.clone())
                            .unwrap_or_default()
                    };
                    let names: Vec<&str> = form.iter().map(|(key, _)| key.as_str()).collect();
                    assert_eq!(
                        names,
                        [
                            "client_id",
                            "client_secret",
                            "redirect_uri",
                            "code",
                            "grant_type"
                        ]
                    );
                    if field("client_id") != "client"
                        || field("client_secret") != "s3cret"
                        || field("redirect_uri")
                            != "https://phab.example/auth/login/bmo:bugzilla.mozilla.org/"
                        || field("grant_type") != "authorization_code"
                    {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    match field("code").strip_prefix("code-") {
                        Some(account) => Json(json!({
                            "access_token": format!("token-{account}"),
                            "expires_in": 3600,
                        }))
                        .into_response(),
                        None => StatusCode::BAD_REQUEST.into_response(),
                    }
                }
            }),
        )
        .route(
            "/api/user/profile",
            routing::get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(headers[header::USER_AGENT], "PhutilBMOAuthAdapter");
                let authorization = headers[header::AUTHORIZATION].to_str().unwrap();
                let account = authorization
                    .strip_prefix("Bearer token-")
                    .unwrap()
                    .to_owned();
                if account == "nomfa" {
                    return Json(json!({"id": 1003, "login": "nomfa@example.com",
                        "nick": "nomfa", "name": "No MFA", "mfa": false}));
                }
                let id: i64 = account.parse().unwrap();
                Json(json!({"id": id, "login": format!("user{id}@example.com"),
                    "nick": format!("nick{id}"), "name": format!("User {id}"), "mfa": true}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeBugzilla {
        url,
        token_requests,
    }
}

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap()
}

/// `admin` (password), `alice` (BMO 1001), `gone` (disabled, BMO 1002)
/// and `mfa` (BMO 1004, with a factor); alice has the session
/// `alice-token`, mfa the partial session `mfa-token`.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&["user", "auth", "system"]).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-admin', 'admin', 'Admin', 1, 1, 'c', 0, 0, 1, 1, 1, '{ACCOUNT_SECRET}', 0, 0), \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, '{ACCOUNT_SECRET}', 0, 0), \
             ('PHID-USER-gone', 'gone', 'Gone', 1, 1, 'c', 0, 1, 0, 1, 1, '{ACCOUNT_SECRET}', 0, 0), \
             ('PHID-USER-mfa', 'mfa', 'Mfa', 1, 1, 'c', 0, 0, 0, 1, 1, '{ACCOUNT_SECRET}', 1, 0); \
             INSERT INTO `{{ns}}_user`.user_email (phid, userPHID, address, isVerified, isPrimary, \
               dateCreated, dateModified) VALUES \
             ('PHID-EADR-1', 'PHID-USER-admin', 'admin@example.com', 1, 1, 1, 1); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), \
               ('csrf', '{CSRF_KEY_HEX}', 1, 1), ('csrf.alternate', '{ALTERNATE_KEY_HEX}', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0), \
             ('PHID-USER-mfa', 'web', '{mfa}', 1, {expires}, 'PHID-SSSN-2', 1); \
             INSERT INTO `{{ns}}_auth`.auth_providerconfig (phid, providerClass, providerType, \
               providerDomain, isEnabled, shouldAllowLogin, shouldAllowRegistration, \
               shouldAllowLink, shouldAllowUnlink, shouldTrustEmails, properties, dateCreated, \
               dateModified, shouldAutoLogin) VALUES \
             ('{BMO_PROVIDER}', 'PhabricatorBMOAuthProvider', 'bmo', 'bugzilla.mozilla.org', \
               1, 1, 1, 1, 1, 0, '{{\"oauth:app:id\":\"client\",\"oauth:app:secret\":\"s3cret\"}}', \
               1, 1, 0), \
             ('{PASSWORD_PROVIDER}', 'PhabricatorPasswordAuthProvider', 'password', 'self', \
               1, 1, 0, 0, 0, 0, '[]', 1, 1, 0); \
             INSERT INTO `{{ns}}_auth`.auth_password (phid, objectPHID, passwordType, \
               passwordHash, isRevoked, dateCreated, dateModified, passwordSalt, \
               legacyDigestFormat) VALUES \
             ('PHID-APAS-1', 'PHID-USER-admin', 'account', '{ADMIN_HASH}', 0, 1, 1, \
               'adminsalt', NULL); \
             INSERT INTO `{{ns}}_auth`.auth_factorprovider (phid, providerFactorKey, status, \
               properties, dateCreated, dateModified, name) VALUES \
             ('PHID-FPRV-1', 'totp', 'active', '{{}}', 1, 1, 'TOTP'); \
             INSERT INTO `{{ns}}_auth`.auth_factorconfig (phid, userPHID, factorName, \
               factorSecret, properties, dateCreated, dateModified, factorProviderPHID) VALUES \
             ('PHID-AFTR-1', 'PHID-USER-mfa', 'phone', 'x', '{{}}', 1, 1, 'PHID-FPRV-1'); \
             ALTER TABLE `{{ns}}_user`.user_externalaccount DROP KEY account_details",
            alice = session_key("alice-token"),
            mfa = session_key("mfa-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    for (account, user, identifier) in [
        ("PHID-XUSR-alice", "PHID-USER-alice", "1001"),
        ("PHID-XUSR-gone", "PHID-USER-gone", "1002"),
        ("PHID-XUSR-mfa", "PHID-USER-mfa", "1004"),
    ] {
        test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_user`.user_externalaccount (phid, userPHID, accountType, \
                   accountDomain, accountSecret, accountID, dateCreated, dateModified, \
                   emailVerified, properties, providerConfigPHID) VALUES \
                 ('{account}', '{user}', 'bmo', 'bugzilla.mozilla.org', 'secret-{identifier}', \
                   '{identifier}', 1, 1, 0, '{{\"registrationKey\":\"old\"}}', '{BMO_PROVIDER}'); \
                 INSERT INTO `{{ns}}_user`.user_externalaccountidentifier (phid, \
                   externalAccountPHID, providerConfigPHID, identifierHash, identifierRaw, \
                   dateCreated, dateModified) VALUES \
                 ('PHID-XIDT-{identifier}', '{account}', '{BMO_PROVIDER}', '{hash}', \
                   '{identifier}', 1, 1)",
                hash = hash::digest_for_index(identifier.as_bytes()),
            ))
            .await;
    }
    Some(test_db)
}

fn config(bugzilla_url: &str) -> Config {
    let mut config = Config::default();
    config.push_layer(
        "test",
        json!({
            "phabricator.base-uri": "https://phab.example/",
            "bugzilla.url": bugzilla_url,
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    config
}

/// The router with Rust login on, or off.
fn app(test_db: &TestDatabase, config: Config, login: bool) -> Router {
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        login: login.then(|| LoginSettings::new(reqwest::Client::new(), "web-1")),
        config,
    });
    phab_web::router::<State>().with_state(state)
}

async fn send(app: &Router, request: axum::http::request::Builder, body: Body) -> Response {
    let request = request
        .header(header::HOST, HOST)
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "6.6.6.6, 10.1.2.3")
        .header(header::USER_AGENT, "Test/1.0")
        .body(body)
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
}

async fn get(app: &Router, path: &str, cookie: &str) -> Response {
    let mut request = Request::get(path);
    if !cookie.is_empty() {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request, Body::empty()).await
}

async fn post(app: &Router, path: &str, cookie: &str, form: &[(&str, &str)]) -> Response {
    let body: String = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let mut request =
        Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if !cookie.is_empty() {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request, Body::from(body)).await
}

fn set_cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

/// The value a `Set-Cookie` header gives `name`, if any.
fn cookie_value(response: &Response, name: &str) -> Option<String> {
    set_cookies(response)
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix(&format!("{name}=")).map(str::to_owned))
        .map(|rest| rest.split(';').next().unwrap().to_owned())
}

fn location(response: &Response) -> &str {
    response.headers()[header::LOCATION].to_str().unwrap()
}

fn assert_fallback(response: &Response, reason: &str) {
    assert_eq!(
        response.status(),
        StatusCode::MISDIRECTED_REQUEST,
        "{reason}"
    );
    assert_eq!(response.headers()[FALLBACK_HEADER], reason);
}

async fn text(response: Response) -> String {
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The one value `sql` selects, read as text and parsed.
async fn scalar<T>(test_db: &TestDatabase, sql: &str) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Debug,
{
    let sql = format!(
        "SELECT CAST(({}) AS CHAR) AS v",
        sql.replace("{ns}", test_db.database().namespace())
    );
    let row = phab_db::sqlx::query(&sql)
        .fetch_one(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    phab_db::rows::text(&row, "v").unwrap().parse().unwrap()
}

/// The OAuth state the login button carries for `phcid`.
fn state_for(client_id: &str) -> String {
    phab_auth::oauth::client_state(&Config::default(), client_id).unwrap()
}

/// A CSRF token for the logged-out visitor with the session cookie
/// `anonymous_session`.
async fn alternate_csrf_token(test_db: &TestDatabase, anonymous_session: &str) -> String {
    let phab_auth::Outcome::Done(secret) =
        CsrfSecret::for_logged_out(test_db.database(), anonymous_session)
            .await
            .unwrap()
    else {
        panic!("no csrf.alternate key");
    };
    csrf::new_token(
        &secret,
        &CsrfKey::new(hex::decode(CSRF_KEY_HEX).unwrap()),
        NOW,
    )
    .unwrap()
}

fn user_csrf_token(user: &str, session_token: &str) -> String {
    let secret = CsrfSecret::for_session(
        &phab_core::Phid::new(user),
        ACCOUNT_SECRET,
        &session_key(session_token),
    );
    csrf::new_token(
        &secret,
        &CsrfKey::new(hex::decode(CSRF_KEY_HEX).unwrap()),
        NOW,
    )
    .unwrap()
}

#[tokio::test]
async fn every_login_route_goes_to_php_when_rust_login_is_off() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), false);
    let state = state_for("cid");
    for path in [
        "/auth/start/",
        "/login/",
        "/auth/loggedout/",
        &format!("/auth/login/bmo:bugzilla.mozilla.org/?code=code-1001&state={state}"),
        "/auth/login/password:self/",
        "/auth/validate/?expect=alice",
        "/auth/finish/",
        "/logout/",
    ] {
        assert_fallback(&get(&app, path, "phcid=cid").await, "Rust login is off");
    }
    assert_fallback(
        &post(&app, "/logout/", "phsid=alice-token", &[("__form__", "1")]).await,
        "Rust login is off",
    );
    assert_eq!(bugzilla.token_requests.load(Ordering::SeqCst), 0);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn the_login_page_sets_the_client_cookies() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);

    let response = get(&app, "/auth/start/?next=%2FD123", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let anonymous = cookie_value(&response, "phsid").unwrap();
    assert!(anonymous.starts_with("A%2F"), "{anonymous}");
    let client_id = cookie_value(&response, "phcid").unwrap();
    assert_eq!(client_id.len(), 16);
    assert_eq!(
        cookie_value(&response, "next_uri").as_deref(),
        Some("1700000000%2C%2FD123")
    );
    for line in set_cookies(&response) {
        assert!(
            line.ends_with("; path=/; domain=phab.example; secure; HttpOnly"),
            "{line}"
        );
    }
    let page = text(response).await;
    let expected_state = state_for(&client_id);
    assert!(page.contains(&format!("state={expected_state}")), "{page}");
    assert!(
        page.contains(&format!(
            "{}/oauth/authorize?client_id=client&#38;scope=user%3Aread",
            bugzilla.url
        )),
        "{page}"
    );
    assert!(page.contains("name=\"password\""));
    assert!(!page.contains("s3cret"));

    // A stale session cookie is cleared first.
    let response = get(&app, "/auth/start/", "phsid=stale-token").await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        location(&response),
        "https://phab.example/auth/start/?cleared=1"
    );
    assert_eq!(cookie_value(&response, "phsid").as_deref(), Some("deleted"));

    // Logged-in visitors and requests from another host are PHP's.
    assert_fallback(
        &get(&app, "/auth/start/", "phsid=alice-token").await,
        "logged in",
    );
    let request = Request::get("/auth/start/")
        .header(header::HOST, "evil.example")
        .header("x-forwarded-proto", "https")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_fallback(&response, "cookies can't be set on this host");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn the_callback_refuses_bad_client_state_before_calling_bugzilla() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    let good = state_for("cid");
    let uri = |query: &str| format!("/auth/login/bmo:bugzilla.mozilla.org/?{query}");

    for (path, cookie, reason) in [
        (
            uri(&format!("code=code-1001&state={good}")),
            "",
            "no client ID cookie",
        ),
        (
            uri(&format!("code=code-1001&state={good}")),
            "phcid=other",
            "a wrong OAuth state",
        ),
        (uri("code=code-1001"), "phcid=cid", "a wrong OAuth state"),
        (
            uri(&format!("code=code-1001&state={}", &good[..39])),
            "phcid=cid",
            "a wrong OAuth state",
        ),
        (uri(&format!("state={good}")), "phcid=cid", "no OAuth code"),
        (
            uri(&format!("error=access_denied&state={good}")),
            "phcid=cid",
            "the OAuth provider returned an error",
        ),
        (
            uri(&format!("code=code-1001&state={good}")),
            "phcid=cid; phsid=alice-token",
            "logged in",
        ),
        (
            uri(&format!("code=code-1001&state={good}")),
            "phcid=cid; phsid=mfa-token",
            "logged in",
        ),
        (
            uri(&format!("code=code-1001&state={good}")),
            "phcid=cid; invite=abc",
            "an invite",
        ),
        (
            format!("/auth/login/github:github.com/?code=code-1001&state={good}"),
            "phcid=cid",
            "an unknown or disabled provider",
        ),
    ] {
        assert_fallback(&get(&app, &path, cookie).await, reason);
    }
    assert_eq!(bugzilla.token_requests.load(Ordering::SeqCst), 0);
    let sessions: i64 = scalar(
        &test_db,
        "SELECT COUNT(*) FROM `{ns}_user`.phabricator_session",
    )
    .await;
    assert_eq!(sessions, 2);
    test_db.drop_databases().await;
}

/// Logs in through BMO with `code`, from a browser with the `phcid` cookie
/// `cid`.
async fn bmo_callback(app: &Router, code: &str) -> Response {
    let state = state_for("cid");
    get(
        app,
        &format!("/auth/login/bmo:bugzilla.mozilla.org/?code={code}&state={state}"),
        "phcid=cid",
    )
    .await
}

#[tokio::test]
async fn bmo_login_issues_a_partial_session_like_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);

    let response = bmo_callback(&app, "code-1001").await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        location(&response),
        "https://phab.example/auth/validate/?expect=alice"
    );
    assert_eq!(cookie_value(&response, "phusr").as_deref(), Some("alice"));
    let token = cookie_value(&response, "phsid").unwrap();
    assert_eq!(token.len(), 40);
    assert!(!token.contains('%'));
    for cleared in ["phreg", "phcid", "invite"] {
        assert_eq!(cookie_value(&response, cleared).as_deref(), Some("deleted"));
    }
    assert!(
        set_cookies(&response)
            .iter()
            .any(|line| line.starts_with(&format!(
                "phsid={token}; expires=Sun, 12 Nov 2028 22:13:20 GMT; Max-Age=157680000; path=/; \
                 domain=phab.example; secure; HttpOnly"
            )))
    );

    // `establishSession()`: a partial web session stored under the digest.
    let digest = session_key(&token);
    let expires: i64 = scalar(
        &test_db,
        &format!(
            "SELECT sessionExpires FROM `{{ns}}_user`.phabricator_session \
             WHERE sessionKey = '{digest}' AND userPHID = 'PHID-USER-alice' AND type = 'web' \
             AND isPartial = 1 AND signedLegalpadDocuments = 0 AND sessionStart = {NOW} \
             AND highSecurityUntil IS NULL AND phid LIKE 'PHID-SSSN-%'"
        ),
    )
    .await;
    assert_eq!(expires, NOW + 30 * 60);
    let log: String = scalar(
        &test_db,
        &format!(
            "SELECT CONCAT_WS('|', action, userPHID, IFNULL(actorPHID, 'NULL'), remoteAddr, \
               session, oldValue, newValue, details, dateCreated) \
             FROM `{{ns}}_user`.user_log WHERE session = '{digest}'"
        ),
    )
    .await;
    assert_eq!(
        log,
        format!(
            "login-partial|PHID-USER-alice|NULL|10.1.2.3|{digest}|null|null|\
             {{\"session_type\":\"web\",\"host\":\"web-1\",\"user_agent\":\"Test\\/1.0\"}}|{NOW}"
        )
    );

    // `didUpdateAccount()` and `synchronizeOAuthAccount()`.
    let account: String = scalar(
        &test_db,
        "SELECT CONCAT_WS('|', username, realName, email, properties, dateModified, \
           accountSecret) FROM `{ns}_user`.user_externalaccount WHERE phid = 'PHID-XUSR-alice'",
    )
    .await;
    assert_eq!(
        account,
        format!(
            "nick1001|User 1001|user1001@example.com|{{\"registrationKey\":\"old\",\
             \"oauth.token.access\":\"token-1001\",\"oauth.token.refresh\":null,\
             \"oauth.token.access.expires\":{}}}|{NOW}|secret-1001",
            NOW + 3600
        )
    );
    let identifier_modified: i64 = scalar(
        &test_db,
        "SELECT dateModified FROM `{ns}_user`.user_externalaccountidentifier \
         WHERE phid = 'PHID-XIDT-1001'",
    )
    .await;
    assert_eq!(identifier_modified, NOW);

    // `/auth/validate/` checks the cookies came back, then `/auth/finish/`
    // makes the session full and goes to the page the user wanted.
    let cookies = format!("phusr=alice; phsid={token}");
    let response = get(&app, "/auth/validate/?expect=alice", &cookies).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(location(&response), "https://phab.example/auth/finish/");
    let response = get(
        &app,
        "/auth/validate/?expect=alice",
        &format!("phsid={token}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(text(response).await.contains("did not accept the cookie"));

    let response = get(
        &app,
        "/auth/finish/",
        &format!("{cookies}; next_uri=1699999000%2C%2FD123"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(location(&response), "https://phab.example/D123");
    assert_eq!(
        cookie_value(&response, "next_uri").as_deref(),
        Some("deleted")
    );
    let partial: i64 = scalar(
        &test_db,
        &format!(
            "SELECT isPartial FROM `{{ns}}_user`.phabricator_session WHERE sessionKey = '{digest}'"
        ),
    )
    .await;
    assert_eq!(partial, 0);
    let full: String = scalar(
        &test_db,
        &format!(
            "SELECT CONCAT_WS('|', userPHID, actorPHID, details) FROM `{{ns}}_user`.user_log \
             WHERE action = 'login-full' AND session = '{digest}'"
        ),
    )
    .await;
    assert_eq!(
        full,
        "PHID-USER-alice|PHID-USER-alice|{\"host\":\"web-1\",\"user_agent\":\"Test\\/1.0\"}"
    );
    // A full session just goes home.
    let response = get(&app, "/auth/finish/", &cookies).await;
    assert_eq!(location(&response), "https://phab.example/");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn finishing_a_login_never_redirects_off_the_site() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    for next in [
        "%2F%2Fevil.example",
        "https%3A%2F%2Fevil.example",
        "%2F%5Cevil.example",
    ] {
        let token = cookie_value(&bmo_callback(&app, "code-1001").await, "phsid").unwrap();
        let response = get(
            &app,
            "/auth/finish/",
            &format!("phsid={token}; next_uri=1%2C{next}"),
        )
        .await;
        assert_eq!(location(&response), "https://phab.example/", "{next}");
    }
    test_db.drop_databases().await;
}

#[tokio::test]
async fn multi_factor_prompts_are_left_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    let response = get(&app, "/auth/finish/", "phsid=mfa-token").await;
    assert_fallback(&response, "multi-factor authentication is pending");
    let response = post(
        &app,
        "/auth/finish/",
        "phsid=mfa-token",
        &[("__hisec__", "1")],
    )
    .await;
    assert_fallback(&response, "a multi-factor form");
    let partial: i64 = scalar(
        &test_db,
        "SELECT isPartial FROM `{ns}_user`.phabricator_session WHERE phid = 'PHID-SSSN-2'",
    )
    .await;
    assert_eq!(partial, 1);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn bmo_login_refuses_accounts_like_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);

    // Bugzilla accounts without multi-factor authentication.
    let response = bmo_callback(&app, "code-nomfa").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(cookie_value(&response, "phusr").is_none());
    let page = text(response).await;
    assert!(
        page.contains("requires multi-factor authentication"),
        "{page}"
    );
    assert!(page.contains(&format!("href=\"{}/userprefs.cgi?tab=mfa\"", bugzilla.url)));

    // A code Bugzilla refuses: the code is spent, so this is not replayed.
    let response = bmo_callback(&app, "bad").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(cookie_value(&response, "phsid").is_none_or(|value| value.starts_with("A%2F")));

    // Disabled users get a session, and every page stops them (as PHP).
    let response = bmo_callback(&app, "code-1002").await;
    assert_eq!(
        location(&response),
        "https://phab.example/auth/validate/?expect=gone"
    );
    let sessions: i64 = scalar(
        &test_db,
        "SELECT COUNT(*) FROM `{ns}_user`.phabricator_session WHERE userPHID = 'PHID-USER-alice' \
           OR userPHID = 'PHID-USER-gone'",
    )
    .await;
    assert_eq!(sessions, 2);
    let gone = cookie_value(&response, "phsid").unwrap();
    assert_fallback(
        &get(&app, "/auth/finish/", &format!("phsid={gone}")).await,
        "a disabled user",
    );

    // An unknown account goes to registration, with a registration key.
    let response = bmo_callback(&app, "code-2000").await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let registration_key = cookie_value(&response, "phreg").unwrap();
    assert_eq!(registration_key.len(), 32);
    let (secret, properties): (String, String) = phab_db::sqlx::query_as(&format!(
        "SELECT CAST(account.accountSecret AS CHAR), CAST(account.properties AS CHAR) FROM {} account JOIN {} identifier \
         ON identifier.externalAccountPHID = account.phid WHERE identifier.identifierRaw = '2000' \
         AND account.userPHID IS NULL AND account.accountType = 'bmo'",
        test_db.database().table("user", "user_externalaccount"),
        test_db.database().table("user", "user_externalaccountidentifier"),
    ))
    .fetch_one(test_db.database().pool())
    .await
    .unwrap();
    assert_eq!(
        location(&response),
        format!("https://phab.example/auth/register/{secret}/")
    );
    let properties: Value = serde_json::from_str(&properties).unwrap();
    assert_eq!(
        properties["registrationKey"],
        hash::weak_digest(
            registration_key.as_bytes(),
            Config::default().hmac_key().unwrap().as_bytes()
        )
        .unwrap()
    );
    assert!(!format!("{properties}").contains(&registration_key));

    // Without registration, unknown accounts are refused.
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_auth`.auth_providerconfig SET shouldAllowRegistration = 0 \
             WHERE phid = '{BMO_PROVIDER}'"
        ))
        .await;
    let response = bmo_callback(&app, "code-2001").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        text(response)
            .await
            .contains("not configured to allow registration")
    );
    test_db.drop_databases().await;
}

/// Gets the password form, and returns the anonymous session cookie and
/// the form's CSRF token.
async fn password_form(app: &Router) -> (String, String) {
    let response = get(app, "/auth/login/password:self/", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let session = cookie_value(&response, "phsid")
        .unwrap()
        .replace("%2F", "/");
    let page = text(response).await;
    let token = page
        .split("name=\"__csrf__\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    (session, token)
}

#[tokio::test]
async fn password_login_for_administrators() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    let (session, csrf_token) = password_form(&app).await;
    // The form's token is the logged-out one of the anonymous session.
    let fresh = alternate_csrf_token(&test_db, &session).await;
    assert_eq!(fresh.len(), csrf_token.len());
    let cookie = format!("phsid={}", session.replace('/', "%2F"));
    let form = |username: &'static str, password: &'static str, token: String| {
        vec![
            ("__form__", "1".to_owned()),
            ("__csrf__", token),
            ("username", username.to_owned()),
            ("password", password.to_owned()),
        ]
    };
    let send_form = async |fields: Vec<(&str, String)>| {
        let pairs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        post(&app, "/auth/login/password:self/", &cookie, &pairs).await
    };

    // A wrong password is PHP's to refuse, as for any other account, so
    // that no answer tells administrators apart.
    for (username, reason) in [
        ("admin", "not an administrator's password"),
        ("alice", "not an administrator's password"),
        ("nobody", "an unknown account"),
        ("", "an unknown account"),
    ] {
        assert_fallback(
            &send_form(form(username, "wrong", csrf_token.clone())).await,
            reason,
        );
    }
    // A user's right password, if they are no administrator.
    test_db
        .execute(
            "INSERT INTO `{ns}_auth`.auth_password (phid, objectPHID, passwordType, \
               passwordHash, isRevoked, dateCreated, dateModified, passwordSalt) \
             SELECT 'PHID-APAS-2', 'PHID-USER-alice', passwordType, passwordHash, 0, 1, 1, \
               passwordSalt FROM `{ns}_auth`.auth_password WHERE phid = 'PHID-APAS-1'",
        )
        .await;
    assert_fallback(
        &send_form(form("alice", "admin-password", csrf_token.clone())).await,
        "not an administrator's password",
    );

    // The right one logs in, by username or email address.
    for username in ["admin", "admin@example.com"] {
        let response = send_form(form(username, "admin-password", csrf_token.clone())).await;
        assert_eq!(response.status(), StatusCode::FOUND, "{username}");
        assert_eq!(
            location(&response),
            "https://phab.example/auth/validate/?expect=admin"
        );
        let token = cookie_value(&response, "phsid").unwrap();
        let partial: i64 = scalar(
            &test_db,
            &format!(
                "SELECT isPartial FROM `{{ns}}_user`.phabricator_session \
                 WHERE sessionKey = '{}' AND userPHID = 'PHID-USER-admin'",
                session_key(&token)
            ),
        )
        .await;
        assert_eq!(partial, 1);
    }
    let accounts: i64 = scalar(
        &test_db,
        &format!(
            "SELECT COUNT(*) FROM `{{ns}}_user`.user_externalaccount \
             WHERE userPHID = 'PHID-USER-admin' AND providerConfigPHID = '{PASSWORD_PROVIDER}' \
             AND accountType = 'password' AND accountDomain = 'self'"
        ),
    )
    .await;
    assert_eq!(accounts, 1);
    let attempts: f64 = scalar(
        &test_db,
        "SELECT SUM(score) FROM `{ns}_system`.system_actionlog \
         WHERE action = 'auth.password' AND actorIdentity = '10.1.2.3'",
    )
    .await;
    // The form and two logins: refused attempts are PHP's to count.
    assert!((attempts - 3.0).abs() < f64::EPSILON);

    // Everything else is PHP's, before anything is written.
    assert_fallback(
        &send_form(form("admin", "admin-password", "B@bogus".to_owned())).await,
        "an invalid CSRF token",
    );
    assert_fallback(
        &post(
            &app,
            "/auth/login/password:self/",
            "",
            &[
                ("__form__", "1"),
                ("username", "admin"),
                ("password", "admin-password"),
            ],
        )
        .await,
        "a form post without a session",
    );
    let token_for_other_session = alternate_csrf_token(&test_db, "A/someoneelse").await;
    assert_fallback(
        &send_form(form("admin", "admin-password", token_for_other_session)).await,
        "an invalid CSRF token",
    );
    test_db
        .execute(
            "UPDATE `{ns}_auth`.auth_password SET passwordHash = 'md5:abc' \
             WHERE objectPHID = 'PHID-USER-admin'",
        )
        .await;
    assert_fallback(
        &send_form(form("admin", "admin-password", csrf_token.clone())).await,
        "a password hasher other than bcrypt",
    );
    test_db
        .execute(
            "INSERT INTO `{ns}_system`.system_actionlog (actorHash, actorIdentity, action, \
             score, epoch) VALUES ('x', 'y', 'z', 0, 0)",
        )
        .await;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_system`.system_actionlog (actorHash, actorIdentity, action, \
             score, epoch) VALUES ('{}', '10.1.2.3', 'auth.password', 100, {NOW})",
            hash::digest_for_index(b"10.1.2.3")
        ))
        .await;
    assert_fallback(
        &get(&app, "/auth/login/password:self/", "").await,
        "too many login attempts",
    );
    let logs_after: i64 = scalar(
        &test_db,
        "SELECT COUNT(*) FROM `{ns}_user`.user_log WHERE action = 'login-fail'",
    )
    .await;
    assert_eq!(logs_after, 0);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn logout_needs_a_valid_form_post() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    let alice_sessions = async || -> i64 {
        scalar(
            &test_db,
            "SELECT COUNT(*) FROM `{ns}_user`.phabricator_session WHERE userPHID = 'PHID-USER-alice'",
        )
        .await
    };

    // The dialog, with a CSRF token for the session.
    let response = get(&app, "/logout/", "phsid=alice-token").await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("Are you sure you want to log out?"));
    assert!(page.contains("name=\"__csrf__\""));
    // A logged-out visitor goes home.
    assert_eq!(
        location(&get(&app, "/logout/", "").await),
        "https://phab.example/"
    );

    // A cross-site post without a valid token logs nobody out.
    assert_fallback(
        &post(
            &app,
            "/logout/",
            "phsid=alice-token",
            &[("__form__", "1"), ("__csrf__", "B@forged000000000000000")],
        )
        .await,
        "an invalid CSRF token",
    );
    assert_fallback(
        &post(
            &app,
            "/logout/",
            "phsid=alice-token",
            &[
                ("__form__", "1"),
                ("__csrf__", &user_csrf_token("PHID-USER-mfa", "mfa-token")),
            ],
        )
        .await,
        "an invalid CSRF token",
    );
    // A post which is not a form post shows the dialog again.
    let response = post(&app, "/logout/", "phsid=alice-token", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(alice_sessions().await, 1);

    let token = user_csrf_token("PHID-USER-alice", "alice-token");
    let response = post(
        &app,
        "/logout/",
        "phsid=alice-token",
        &[("__form__", "1"), ("__csrf__", &token)],
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FOUND,
        "{:?}",
        response.headers()
    );
    assert_eq!(location(&response), "https://phab.example/auth/loggedout/");
    assert_eq!(cookie_value(&response, "phsid").as_deref(), Some("deleted"));
    assert_eq!(alice_sessions().await, 0);
    let logout: String = scalar(
        &test_db,
        "SELECT CONCAT_WS('|', userPHID, actorPHID, session) FROM `{ns}_user`.user_log \
         WHERE action = 'logout'",
    )
    .await;
    assert_eq!(
        logout,
        format!(
            "PHID-USER-alice|PHID-USER-alice|{}",
            session_key("alice-token")
        )
    );

    // After logging out, the login page doesn't log back in automatically.
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_auth`.auth_providerconfig SET shouldAutoLogin = 1 \
             WHERE phid = '{BMO_PROVIDER}'; \
             UPDATE `{{ns}}_auth`.auth_providerconfig SET isEnabled = 0 \
             WHERE phid = '{PASSWORD_PROVIDER}'"
        ))
        .await;
    let response = get(&app, "/auth/loggedout/", "phcid=cid").await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = get(&app, "/auth/start/", "phcid=cid").await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert!(location(&response).starts_with(&format!(
        "{}/oauth/authorize?client_id=client&scope=user%3Aread",
        bugzilla.url
    )));
    assert!(
        location(&response).ends_with(&format!("&state={}&response_type=code", state_for("cid")))
    );
    test_db.drop_databases().await;
}
