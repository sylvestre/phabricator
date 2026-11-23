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
/// The RFC 6238 test secret, in base32: the `mfa` user's TOTP factor.
const TOTP_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
const CHALLENGE_KEY_HEX: &str = "c0ffee";
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
    setup_with(&[]).await
}

/// [`setup()`] with more application databases.
async fn setup_with(extra: &[&str]) -> Option<TestDatabase> {
    let mut applications = vec!["user", "auth", "system"];
    applications.extend_from_slice(extra);
    let test_db = TestDatabase::create(&applications).await?;
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
               ('csrf', '{CSRF_KEY_HEX}', 1, 1), ('csrf.alternate', '{ALTERNATE_KEY_HEX}', 1, 1), \
               ('auth.challenge.token', '{CHALLENGE_KEY_HEX}', 1, 1); \
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
             ('PHID-AFTR-1', 'PHID-USER-mfa', 'phone', '{TOTP_SECRET}', '[]', 1, 1, 'PHID-FPRV-1'); \
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
    // Errors of pages are only logged.
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
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

#[track_caller]
fn location(response: &Response) -> &str {
    response
        .headers()
        .get(header::LOCATION)
        .unwrap_or_else(|| {
            panic!(
                "no redirect: {} {:?}",
                response.status(),
                response.headers()
            )
        })
        .to_str()
        .unwrap()
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
        "/auth/link/1/",
        "/auth/refresh/1/",
        "/auth/confirmlink/secret-1001/",
        "/auth/register/secret-1001/",
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
            "phcid=cid; phsid=mfa-token",
            "a partial session",
        ),
        (
            uri(&format!("code=code-1001&state={good}")),
            "phcid=other; phsid=alice-token",
            "a wrong OAuth state",
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

/// The TOTP code of the test factor for `timestep`.
fn totp_code(timestep: i64) -> String {
    phab_auth::totp::TotpSecret::new(TOTP_SECRET)
        .unwrap()
        .code(timestep)
}

/// The value of the hidden field `name` of a page.
fn hidden_field(page: &str, name: &str) -> Option<String> {
    page.split(&format!("name=\"{name}\" value=\""))
        .nth(1)
        .map(|rest| rest.split('"').next().unwrap().replace("&#58;", ":"))
}

/// Rows of a table, as one text line each, in ID order.
async fn rows(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query_scalar::<_, Vec<u8>>(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .into_iter()
        .map(|row| String::from_utf8(row).unwrap())
        .collect()
}

const CHALLENGE_ROW: &str = "SELECT CONCAT_WS('|', userPHID, factorPHID, sessionPHID, \
    workflowKey, challengeKey, challengeTTL, IFNULL(responseDigest, 'NULL'), \
    IFNULL(responseTTL, 'NULL'), isCompleted, properties, dateCreated, dateModified, \
    LEFT(phid, 10)) FROM `{ns}_auth`.auth_challenge ORDER BY id";
const ACTION_ROWS: &str = "SELECT CONCAT_WS('|', actorIdentity, action, score, epoch) \
    FROM `{ns}_system`.system_actionlog ORDER BY id";
const HISEC_LOG: &str = "SELECT CONCAT_WS('|', action, actorPHID, userPHID, session, details) \
    FROM `{ns}_user`.user_log WHERE action LIKE 'hisec-%' OR action = 'login-full' ORDER BY id";

#[tokio::test]
async fn totp_factors_finish_the_login_like_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    let step = NOW / 30;
    let cookie = "phsid=mfa-token";
    let csrf = user_csrf_token("PHID-USER-mfa", "mfa-token");

    // The prompt issues one challenge for the current timestep, bound to
    // the session, alive for three timesteps.
    let response = get(&app, "/auth/finish/", cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let page = text(response).await;
    assert!(
        page.contains("name=\"authfactor.1.mfa.response\""),
        "{page}"
    );
    assert!(page.contains("name=\"__hisec__\""));
    assert!(page.contains("Factor Name: phone"));
    assert!(!page.contains(TOTP_SECRET));
    let challenge = format!(
        "PHID-USER-mfa|PHID-AFTR-1|PHID-SSSN-2|legacy|{step}|{}|NULL|NULL|0|[]|{NOW}|{NOW}|PHID-CHAL-",
        NOW + 90
    );
    assert_eq!(
        rows(&test_db, CHALLENGE_ROW).await,
        std::slice::from_ref(&challenge)
    );
    // Showing it again issues nothing new.
    get(&app, "/auth/finish/", cookie).await;
    assert_eq!(
        rows(&test_db, CHALLENGE_ROW).await,
        std::slice::from_ref(&challenge)
    );

    // A POST without a valid CSRF token is PHP's (it throws), before any
    // write.
    let response = post(
        &app,
        "/auth/finish/",
        cookie,
        &[
            ("__form__", "1"),
            ("__hisec__", "1"),
            ("__csrf__", "B@forgedforgedforgedforged"),
            ("authfactor.1.mfa.response", &totp_code(step)),
        ],
    )
    .await;
    assert_fallback(&response, "an invalid CSRF token");
    assert_eq!(rows(&test_db, ACTION_ROWS).await, Vec::<String>::new());

    // A wrong code counts against the rate limit and is logged.
    let answer = |code: String| {
        vec![
            ("__form__", "1".to_owned()),
            ("__hisec__", "1".to_owned()),
            ("__csrf__", csrf.clone()),
            ("authfactor.1.mfa.response", code),
        ]
    };
    let send = async |cookie: &str, fields: Vec<(&str, String)>| {
        let pairs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        post(&app, "/auth/finish/", cookie, &pairs).await
    };
    // A code two timesteps away is outside the window.
    let response = send(cookie, answer(totp_code(step + 2))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains(">Invalid<"), "{page}");
    assert_eq!(
        rows(&test_db, ACTION_ROWS).await,
        [format!("PHID-USER-mfa|auth.factor|1|{NOW}")]
    );
    let digest = session_key("mfa-token");
    let fail = format!(
        "hisec-fail|PHID-USER-mfa|PHID-USER-mfa|{digest}|\
         {{\"host\":\"web-1\",\"user_agent\":\"Test\\/1.0\"}}"
    );
    assert_eq!(rows(&test_db, HISEC_LOG).await, std::slice::from_ref(&fail));
    // An empty answer is required, and is no attempt.
    let page = text(send(cookie, answer(String::new())).await).await;
    assert!(page.contains(">Required<"), "{page}");
    assert_eq!(rows(&test_db, ACTION_ROWS).await.len(), 1);

    // The right code (from the next timestep, within the window) answers
    // the challenge, completes it, credits the attempt back and makes the
    // session full.
    let response = send(cookie, answer(format!(" {} ", totp_code(step + 1)))).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(location(&response), "https://phab.example/");
    let completed: Vec<String> = rows(&test_db, CHALLENGE_ROW).await;
    let [completed] = completed.as_slice() else {
        panic!("{completed:?}");
    };
    let parts: Vec<&str> = completed.split('|').collect();
    assert_eq!(parts[6].len(), 64, "a SHA-256 digest of the response token");
    assert_eq!(parts[7], (NOW + 60).to_string());
    assert_eq!(parts[8], "1");
    assert_eq!(parts[9], format!("{{\"totp.timestep\":{}}}", step + 1));
    assert_eq!(
        rows(&test_db, ACTION_ROWS).await,
        [
            format!("PHID-USER-mfa|auth.factor|1|{NOW}"),
            format!("PHID-USER-mfa|auth.factor|1|{NOW}"),
            format!("PHID-USER-mfa|auth.factor|-1|{NOW}"),
        ]
    );
    let partial: i64 = scalar(
        &test_db,
        "SELECT isPartial FROM `{ns}_user`.phabricator_session WHERE phid = 'PHID-SSSN-2'",
    )
    .await;
    assert_eq!(partial, 0);
    assert_eq!(
        rows(&test_db, HISEC_LOG).await,
        [
            fail.clone(),
            fail.clone(),
            format!(
                "login-full|PHID-USER-mfa|PHID-USER-mfa|{digest}|\
                 {{\"host\":\"web-1\",\"user_agent\":\"Test\\/1.0\"}}"
            ),
        ]
    );

    // Replays: another partial session of the user can't use the code
    // again, nor get a challenge while the completed one is alive.
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.phabricator_session (userPHID, type, sessionKey, \
               sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-mfa', 'web', '{}', 1, {}, 'PHID-SSSN-3', 1)",
            session_key("mfa2-token"),
            NOW + 600,
        ))
        .await;
    let other = "phsid=mfa2-token";
    let page = text(get(&app, "/auth/finish/", other).await).await;
    assert!(
        page.contains("recently issued a challenge to a different login session. Wait 91 seconds"),
        "{page}"
    );
    let other_csrf = user_csrf_token("PHID-USER-mfa", "mfa2-token");
    let mut replay = answer(totp_code(step + 1));
    replay[2].1 = other_csrf;
    let response = send(other, replay).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(text(response).await.contains("Wait 91 seconds"));
    assert_eq!(rows(&test_db, CHALLENGE_ROW).await.len(), 1);
    // A wait is no attempt: the rate limit doesn't move.
    assert_eq!(rows(&test_db, ACTION_ROWS).await.len(), 3);
    let partial: i64 = scalar(
        &test_db,
        "SELECT isPartial FROM `{ns}_user`.phabricator_session WHERE phid = 'PHID-SSSN-3'",
    )
    .await;
    assert_eq!(partial, 1);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn totp_answers_carry_over_and_jump_into_high_security() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    let step = NOW / 30;
    // A second factor: one right answer and one wrong one keep the right
    // one's token in the form.
    test_db
        .execute(
            "INSERT INTO `{ns}_auth`.auth_factorconfig (phid, userPHID, factorName, \
               factorSecret, properties, dateCreated, dateModified, factorProviderPHID) VALUES \
             ('PHID-AFTR-2', 'PHID-USER-mfa', 'tablet', 'MZXW6YTBOI', '[]', 1, 1, \
               'PHID-FPRV-1')",
        )
        .await;
    let cookie = "phsid=mfa-token; jump_to_hisec=1";
    let csrf = user_csrf_token("PHID-USER-mfa", "mfa-token");
    get(&app, "/auth/finish/", cookie).await;
    let response = post(
        &app,
        "/auth/finish/",
        cookie,
        &[
            ("__form__", "1"),
            ("__hisec__", "1"),
            ("__csrf__", &csrf),
            ("authfactor.1.mfa.response", &totp_code(step)),
            ("authfactor.2.mfa.response", "000000"),
        ],
    )
    .await;
    let page = text(response).await;
    assert!(
        page.contains("You responded to this challenge correctly."),
        "{page}"
    );
    let tokens = hidden_field(&page, "__hisec.challenges__").unwrap();
    assert!(tokens.starts_with("PHID-CHAL-"), "{tokens}");

    // A forged token is PHP's exception page.
    let (phid, _) = tokens.split_once(':').unwrap();
    let response = post(
        &app,
        "/auth/finish/",
        cookie,
        &[
            ("__form__", "1"),
            ("__hisec__", "1"),
            ("__csrf__", &csrf),
            ("__hisec.challenges__", &format!("{phid}:forged")),
        ],
    )
    .await;
    assert_fallback(&response, "a forged multi-factor challenge token");

    // Without the token, the answered challenge is a replay.
    let page = text(
        post(
            &app,
            "/auth/finish/",
            cookie,
            &[("__form__", "1"), ("__hisec__", "1"), ("__csrf__", &csrf)],
        )
        .await,
    )
    .await;
    assert!(page.contains("Responses may not be reused"), "{page}");

    // With it, the second factor's code finishes the login, straight into
    // high security.
    let second = phab_auth::totp::TotpSecret::new("MZXW6YTBOI")
        .unwrap()
        .code(step);
    let response = post(
        &app,
        "/auth/finish/",
        cookie,
        &[
            ("__form__", "1"),
            ("__hisec__", "1"),
            ("__csrf__", &csrf),
            ("__hisec.challenges__", &tokens),
            ("authfactor.2.mfa.response", &second),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        cookie_value(&response, "jump_to_hisec").as_deref(),
        Some("deleted")
    );
    let session: String = scalar(
        &test_db,
        "SELECT CONCAT_WS('|', isPartial, highSecurityUntil) \
         FROM `{ns}_user`.phabricator_session WHERE phid = 'PHID-SSSN-2'",
    )
    .await;
    assert_eq!(session, format!("0|{}", NOW + 900));
    let completed: i64 = scalar(
        &test_db,
        "SELECT SUM(isCompleted) FROM `{ns}_auth`.auth_challenge",
    )
    .await;
    assert_eq!(completed, 2);
    let actions: Vec<String> = rows(&test_db, HISEC_LOG)
        .await
        .iter()
        .map(|row| row.split('|').next().unwrap().to_owned())
        .collect();
    assert_eq!(
        actions,
        ["hisec-fail", "hisec-fail", "hisec-enter", "login-full"]
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn multi_factor_paths_php_handles_are_left_to_it() {
    let Some(test_db) = setup().await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    let csrf = user_csrf_token("PHID-USER-mfa", "mfa-token");
    let hisec_post = [
        ("__form__", "1"),
        ("__hisec__", "1"),
        ("__csrf__", csrf.as_str()),
        ("authfactor.1.mfa.response", "123456"),
    ];

    // Ten failed answers in the last hour: PHP's rate limit page, before
    // any challenge is issued.
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_system`.system_actionlog (actorHash, actorIdentity, action, \
               score, epoch) VALUES ('{}', 'PHID-USER-mfa', 'auth.factor', 11, {})",
            hash::digest_for_index(b"PHID-USER-mfa"),
            NOW - 60
        ))
        .await;
    assert_fallback(
        &get(&app, "/auth/finish/", "phsid=mfa-token").await,
        "too many multi-factor attempts",
    );
    assert_fallback(
        &post(&app, "/auth/finish/", "phsid=mfa-token", &hisec_post).await,
        "too many multi-factor attempts",
    );
    test_db
        .execute("DELETE FROM `{ns}_system`.system_actionlog")
        .await;

    // The challenge token key PHP would create.
    test_db
        .execute("DELETE FROM `{ns}_auth`.auth_hmackey WHERE keyName = 'auth.challenge.token'")
        .await;
    assert_fallback(
        &post(&app, "/auth/finish/", "phsid=mfa-token", &hisec_post).await,
        "the challenge token key does not exist yet",
    );

    // Factors other than TOTP.
    test_db
        .execute("UPDATE `{ns}_auth`.auth_factorprovider SET providerFactorKey = 'sms'")
        .await;
    assert_fallback(
        &get(&app, "/auth/finish/", "phsid=mfa-token").await,
        "a multi-factor factor other than TOTP",
    );
    assert_eq!(
        scalar::<i64>(&test_db, "SELECT COUNT(*) FROM `{ns}_auth`.auth_challenge").await,
        0
    );
    assert_eq!(rows(&test_db, ACTION_ROWS).await, Vec::<String>::new());
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

/// The databases registration and linking write to or read policies from.
const LINK_DATABASES: &[&str] = &["worker", "metamta", "spaces", "project"];

/// The anonymous session and registration key a BMO callback for an
/// unknown account hands out, and where it sends the browser.
async fn start_registration(app: &Router, code: &str) -> (String, String, String) {
    let response = bmo_callback(app, code).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let session = cookie_value(&response, "phsid").unwrap();
    let key = cookie_value(&response, "phreg").unwrap();
    let target = location(&response)
        .strip_prefix("https://phab.example")
        .unwrap()
        .to_owned();
    (session, key, target)
}

const USER_ROW: &str = "SELECT CONCAT_WS('|', userName, realName, isEmailVerified, isApproved, \
    isAdmin, isDisabled, isSystemAgent, isMailingList, isEnrolledInMultiFactor, \
    LENGTH(conduitCertificate), LENGTH(accountSecret), IFNULL(profileImagePHID, 'NULL'), \
    dateCreated, dateModified, LEFT(phid, 10)) FROM `{ns}_user`.user WHERE userName = 'newbie'";
const TASK_ROWS: &str = "SELECT CONCAT_WS('|', task.taskClass, task.priority, \
    IFNULL(task.objectPHID, 'NULL'), data.data) FROM `{ns}_worker`.worker_activetask task \
    JOIN `{ns}_worker`.worker_taskdata data ON data.id = task.dataID ORDER BY task.id";

#[tokio::test]
async fn bmo_registration_creates_accounts_like_php() {
    let Some(test_db) = setup_with(LINK_DATABASES).await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_auth`.auth_providerconfig SET shouldTrustEmails = 1 \
             WHERE phid = '{BMO_PROVIDER}'"
        ))
        .await;
    let (session, key, target) = start_registration(&app, "code-2000").await;
    assert!(target.starts_with("/auth/register/"), "{target}");
    let cookie = format!("phsid={session}; phreg={key}");

    // The browser which started the registration gets the form.
    let response = get(&app, &target, &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("value=\"nick2000\""), "{page}");
    assert!(!page.contains("name=\"email\""), "the address is BMO's");
    let csrf = hidden_field(&page, "__csrf__").unwrap();

    // Another browser, or a forged key, gets PHP's errors.
    let page = text(get(&app, &target, &format!("phsid={session}")).await).await;
    assert!(page.contains("did not submit a registration key"), "{page}");
    let page = text(get(&app, &target, &format!("phsid={session}; phreg=forged")).await).await;
    assert!(page.contains("different registration key"), "{page}");
    let page = text(get(&app, "/auth/register/nosuchsecret/", &cookie).await).await;
    assert!(page.contains("No valid linkable account."), "{page}");

    let form = |username: &str| {
        vec![
            ("__form__", "1".to_owned()),
            ("__csrf__", csrf.clone()),
            ("phase", "2".to_owned()),
            ("username", username.to_owned()),
            ("realName", "New Bie".to_owned()),
        ]
    };
    let send = async |fields: Vec<(&str, String)>| {
        let pairs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        post(&app, &target, &cookie, &pairs).await
    };
    // Invalid and taken usernames: the form again, nothing written.
    let page = text(send(form("new bie")).await).await;
    assert!(
        page.contains("Usernames must contain only numbers"),
        "{page}"
    );
    let page = text(send(form("ALICE")).await).await;
    assert!(
        page.contains("Another user already has that username."),
        "{page}"
    );
    let mut forged = form("newbie");
    forged[1].1 = "B@forgedforgedforgedforged".to_owned();
    assert_fallback(&send(forged).await, "an invalid CSRF token");
    assert_eq!(
        scalar::<i64>(&test_db, "SELECT COUNT(*) FROM `{ns}_user`.user").await,
        4
    );

    let response = send(form("newbie")).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        location(&response),
        "https://phab.example/auth/validate/?expect=newbie"
    );
    assert_eq!(cookie_value(&response, "phreg").as_deref(), Some("deleted"));
    let token = cookie_value(&response, "phsid").unwrap();

    // `auth.require-approval` is on by default: unapproved, and the
    // administrators get mail.
    let user: String = scalar(&test_db, USER_ROW).await;
    assert_eq!(
        user,
        format!("newbie|New Bie|1|0|0|0|0|0|0|255|64|NULL|{NOW}|{NOW}|PHID-USER-")
    );
    let user_phid: String = scalar(
        &test_db,
        "SELECT phid FROM `{ns}_user`.user WHERE userName = 'newbie'",
    )
    .await;
    let email: String = scalar(
        &test_db,
        &format!(
            "SELECT CONCAT_WS('|', address, isVerified, isPrimary, LENGTH(verificationCode), \
               dateCreated, LEFT(phid, 10)) FROM `{{ns}}_user`.user_email \
             WHERE userPHID = '{user_phid}'"
        ),
    )
    .await;
    assert_eq!(
        email,
        format!("user2000@example.com|1|1|24|{NOW}|PHID-EADR-")
    );
    let tokens = rows(
        &test_db,
        "SELECT token FROM `{ns}_user`.user_nametoken ORDER BY token",
    )
    .await;
    assert_eq!(tokens, ["bie", "new", "newbie"]);
    let linked: String = scalar(
        &test_db,
        "SELECT account.userPHID FROM `{ns}_user`.user_externalaccount account \
         JOIN `{ns}_user`.user_externalaccountidentifier identifier \
         ON identifier.externalAccountPHID = account.phid WHERE identifier.identifierRaw = '2000'",
    )
    .await;
    assert_eq!(linked, user_phid);
    let mail_id: i64 = scalar(&test_db, "SELECT id FROM `{ns}_metamta`.metamta_mail").await;
    assert_eq!(
        rows(&test_db, TASK_ROWS).await,
        [
            format!(
                "PhabricatorSearchWorker|3500|{user_phid}|{{\"documentPHID\":\"{user_phid}\",\
                 \"parameters\":[],\"strict\":false}}"
            ),
            "PhabricatorRepositoryIdentityChangeWorker|2000|NULL|\
             {\"emailAddresses\":[\"user2000@example.com\"]}"
                .to_owned(),
            format!("PhabricatorMetaMTAWorker|1000|NULL|{mail_id}"),
        ]
    );
    let mail: String = scalar(
        &test_db,
        "SELECT CONCAT_WS('|', IFNULL(actorPHID, 'NULL'), status, IFNULL(relatedPHID, 'NULL'), \
           parameters) FROM `{ns}_metamta`.metamta_mail",
    )
    .await;
    assert_eq!(
        mail,
        "NULL|queued|NULL|{\"sensitive\":true,\"mustEncrypt\":false,\
         \"to\":[\"PHID-USER-admin\"],\"subject\":\"[Phabricator] New User \\\"newbie\\\" \
         Awaiting Approval\",\"body\":\"Newly registered user \\\"newbie\\\" is awaiting \
         account approval by an administrator.\\n\\nAPPROVAL QUEUE\\n  \
         https:\\/\\/phab.example\\/people\\/query\\/approval\\/\\n\\nDISABLE APPROVAL QUEUE\\n  \
         https:\\/\\/phab.example\\/config\\/edit\\/auth.require-approval\\/\\n\"}"
    );
    let edges = rows(
        &test_db,
        "SELECT CONCAT_WS('|', type, dst, seq) FROM `{ns}_metamta`.edge",
    )
    .await;
    assert_eq!(edges, ["57|PHID-USER-admin|0"]);
    let partial: i64 = scalar(
        &test_db,
        &format!(
            "SELECT isPartial FROM `{{ns}}_user`.phabricator_session WHERE sessionKey = '{}'",
            session_key(&token)
        ),
    )
    .await;
    assert_eq!(partial, 1);

    // The registration can't be replayed: the account is linked now.
    let page = text(get(&app, &target, &cookie).await).await;
    assert!(page.contains("already linked to another user"), "{page}");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn registrations_php_writes_differently_are_left_to_it() {
    let Some(test_db) = setup_with(LINK_DATABASES).await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    let (session, key, target) = start_registration(&app, "code-2001").await;
    let cookie = format!("phsid={session}; phreg={key}");
    let csrf = hidden_field(&text(get(&app, &target, &cookie).await).await, "__csrf__").unwrap();
    // BMO addresses aren't trusted here: PHP mails a verification link.
    let response = post(
        &app,
        &target,
        &cookie,
        &[
            ("__form__", "1"),
            ("__csrf__", &csrf),
            ("phase", "2"),
            ("username", "other"),
            ("realName", "Other"),
        ],
    )
    .await;
    assert_fallback(&response, "an address to verify by mail");
    assert_eq!(
        scalar::<i64>(&test_db, "SELECT COUNT(*) FROM `{ns}_user`.user").await,
        4
    );
    assert_eq!(rows(&test_db, TASK_ROWS).await, Vec::<String>::new());
    // Invites and registrations without an account key are PHP's.
    assert_fallback(
        &get(&app, &target, &format!("{cookie}; invite=abc")).await,
        "an invite",
    );
    assert_fallback(
        &get(&app, "/auth/register/", &cookie).await,
        "a registration without an account key",
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn logged_in_users_link_and_refresh_bmo_accounts() {
    let Some(test_db) = setup_with(LINK_DATABASES).await else {
        return;
    };
    let bugzilla = fake_bugzilla().await;
    let app = app(&test_db, config(&bugzilla.url), true);
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.phabricator_session (userPHID, type, sessionKey, \
               sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-admin', 'web', '{}', 1, {expires}, 'PHID-SSSN-3', 0), \
             ('PHID-USER-mfa', 'web', '{}', 1, {expires}, 'PHID-SSSN-4', 0)",
            session_key("admin-token"),
            session_key("mfa-full-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;

    // Alice's BMO account is linked: she can refresh it, not link another.
    let page = text(get(&app, "/auth/link/1/", "phsid=alice-token").await).await;
    assert!(page.contains("Account Already Linked"), "{page}");
    let response = get(&app, "/auth/refresh/1/", "phsid=alice-token").await;
    assert_eq!(response.status(), StatusCode::OK);
    let client_id = cookie_value(&response, "phcid").unwrap();
    let page = text(response).await;
    assert!(page.contains("Refresh Account Link"), "{page}");
    assert!(
        page.contains(&format!("state={}", state_for(&client_id))),
        "{page}"
    );
    let page = text(get(&app, "/auth/refresh/1/", "phsid=admin-token").await).await;
    assert!(page.contains("No Account Linked"), "{page}");
    // Logged out, or with Rust login off, these are PHP's.
    assert_fallback(&get(&app, "/auth/link/1/", "").await, "logged out");
    let off = super_app_off(&test_db, &bugzilla.url);
    assert_fallback(
        &get(&off, "/auth/link/1/", "phsid=admin-token").await,
        "Rust login is off",
    );

    // The refresh: Bugzilla sends Alice back with her own account.
    let state = state_for("cid");
    let callback =
        |code: &str| format!("/auth/login/bmo:bugzilla.mozilla.org/?code={code}&state={state}");
    let response = get(&app, &callback("code-1001"), "phcid=cid; phsid=alice-token").await;
    assert_eq!(
        location(&response),
        "https://phab.example/settings/panel/external/"
    );
    // Someone else's account can't be linked.
    let page = text(get(&app, &callback("code-1001"), "phcid=cid; phsid=admin-token").await).await;
    assert!(page.contains("already associated with another"), "{page}");

    // A new account goes to the confirmation, which links it once the
    // viewer confirms with a form post.
    let response = get(&app, &callback("code-3000"), "phcid=cid; phsid=admin-token").await;
    let target = location(&response)
        .strip_prefix("https://phab.example")
        .unwrap()
        .to_owned();
    assert!(target.starts_with("/auth/confirmlink/"), "{target}");
    let key = cookie_value(&response, "phreg").unwrap();
    let cookie = format!("phsid=admin-token; phreg={key}");
    let page = text(get(&app, &target, &cookie).await).await;
    assert!(page.contains("Confirm Account Link"), "{page}");
    assert!(page.contains("nick3000"), "{page}");
    let page = text(get(&app, &target, "phsid=admin-token").await).await;
    assert!(page.contains("did not submit a registration key"), "{page}");
    let csrf = user_csrf_token("PHID-USER-admin", "admin-token");
    let response = post(
        &app,
        &target,
        &cookie,
        &[("__form__", "1"), ("__csrf__", &csrf)],
    )
    .await;
    assert_eq!(
        location(&response),
        "https://phab.example/settings/panel/external/"
    );
    assert_eq!(cookie_value(&response, "phreg").as_deref(), Some("deleted"));
    let linked: String = scalar(
        &test_db,
        "SELECT account.userPHID FROM `{ns}_user`.user_externalaccount account \
         JOIN `{ns}_user`.user_externalaccountidentifier identifier \
         ON identifier.externalAccountPHID = account.phid WHERE identifier.identifierRaw = '3000'",
    )
    .await;
    assert_eq!(linked, "PHID-USER-admin");

    // A user with a TOTP factor confirms with a code, for this link only.
    // (Their BMO account is unlinked first: one account per provider.)
    test_db
        .execute(
            "UPDATE `{ns}_user`.user_externalaccount SET userPHID = NULL \
             WHERE phid = 'PHID-XUSR-mfa'",
        )
        .await;
    let response = get(
        &app,
        &callback("code-3001"),
        "phcid=cid; phsid=mfa-full-token",
    )
    .await;
    let target = location(&response)
        .strip_prefix("https://phab.example")
        .unwrap()
        .to_owned();
    let key = cookie_value(&response, "phreg").unwrap();
    let cookie = format!("phsid=mfa-full-token; phreg={key}");
    let csrf = user_csrf_token("PHID-USER-mfa", "mfa-full-token");
    let page = text(
        post(
            &app,
            &target,
            &cookie,
            &[("__form__", "1"), ("__csrf__", &csrf)],
        )
        .await,
    )
    .await;
    assert!(page.contains("Provide MFA Credentials"), "{page}");
    assert!(
        page.contains("name=\"authfactor.1.mfa.response\""),
        "{page}"
    );
    let account_phid: String = scalar(
        &test_db,
        "SELECT account.phid FROM `{ns}_user`.user_externalaccount account \
         JOIN `{ns}_user`.user_externalaccountidentifier identifier \
         ON identifier.externalAccountPHID = account.phid WHERE identifier.identifierRaw = '3001'",
    )
    .await;
    let workflow: String = scalar(
        &test_db,
        "SELECT workflowKey FROM `{ns}_auth`.auth_challenge",
    )
    .await;
    assert_eq!(workflow, format!("account.link({account_phid})"));
    let response = post(
        &app,
        &target,
        &cookie,
        &[
            ("__form__", "1"),
            ("__hisec__", "1"),
            ("__csrf__", &csrf),
            ("authfactor.1.mfa.response", &totp_code(NOW / 30)),
        ],
    )
    .await;
    assert_eq!(
        location(&response),
        "https://phab.example/settings/panel/external/"
    );
    let linked: String = scalar(
        &test_db,
        &format!(
            "SELECT userPHID FROM `{{ns}}_user`.user_externalaccount WHERE phid = '{account_phid}'"
        ),
    )
    .await;
    assert_eq!(linked, "PHID-USER-mfa");
    // A token for this action, not high security for the session.
    let until: String = scalar(
        &test_db,
        "SELECT IFNULL(highSecurityUntil, 'NULL') FROM `{ns}_user`.phabricator_session \
         WHERE phid = 'PHID-SSSN-4'",
    )
    .await;
    assert_eq!(until, "NULL");
    test_db.drop_databases().await;
}

/// The router with Rust login off.
fn super_app_off(test_db: &TestDatabase, bugzilla_url: &str) -> Router {
    app(test_db, config(bugzilla_url), false)
}
