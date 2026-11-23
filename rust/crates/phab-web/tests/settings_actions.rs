//! The actions of the settings list panels against a test database:
//! Conduit API tokens, sessions and email addresses, their dialogs, the
//! high security prompt and the rows each writes.

// Few databases, many scenarios each: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::{FALLBACK_HEADER, WebState};
use serde_json::{Value, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
const ALICE: &str = "phsid=alice-token; phab_ui=rust";
const ADMIN: &str = "phsid=admin-token; phab_ui=rust";

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

    fn php(&self) -> Option<&phab_web::php::PhpConduit> {
        None
    }

    fn hostname(&self) -> Option<&str> {
        Some("rust-host")
    }
}

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap()
}

/// Users `alice`, `bob`, `admin` (an administrator), `bot` and the
/// mailing list `list`; sessions `alice-token` (1), `admin-token` (2) and
/// two other sessions of alice (3, 4); addresses and tokens of alice and
/// bob.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "user", "auth", "worker", "herald", "conduit", "spaces", "project", "system", "metamta",
        "flag", "feed", "search",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-admin', 'admin', 'Admin', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0), \
             ('PHID-USER-bot', 'bot', 'Bot', 1, 1, 'c', 1, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-list', 'list', 'List', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 1); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (id, userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial, \
                highSecurityUntil) VALUES \
             (1, 'PHID-USER-alice', 'web', '{alice}', 1699000000, {expires}, 'PHID-SSSN-1', 0, \
              NULL), \
             (2, 'PHID-USER-admin', 'web', '{admin}', 1, {expires}, 'PHID-SSSN-2', 0, NULL), \
             (3, 'PHID-USER-alice', 'conduit', 'abcdef0123', 1699500000, {expires}, \
              'PHID-SSSN-3', 0, NULL), \
             (4, 'PHID-USER-alice', 'web', '0123456789', 1699500000, {expires}, 'PHID-SSSN-4', \
              0, NULL); \
             INSERT INTO `{{ns}}_user`.user_email (id, phid, userPHID, address, isVerified, \
               isPrimary, verificationCode, dateCreated, dateModified) VALUES \
             (1, 'PHID-EADR-1', 'PHID-USER-alice', 'b@example.com', 0, 0, 'codeb', 1, 1), \
             (2, 'PHID-EADR-2', 'PHID-USER-alice', 'a@example.com', 1, 1, 'codea', 1, 1), \
             (3, 'PHID-EADR-3', 'PHID-USER-alice', 'c@example.com', 1, 0, NULL, 1, 1), \
             (4, 'PHID-EADR-4', 'PHID-USER-bob', 'bob@example.com', 1, 1, 'codebob', 1, 1); \
             INSERT INTO `{{ns}}_conduit`.conduit_token (id, objectPHID, tokenType, token, \
               expires, dateCreated, dateModified) VALUES \
             (1, 'PHID-USER-alice', 'api', 'api-abcdefghijklmnop', NULL, 1, 1), \
             (2, 'PHID-USER-alice', 'cli', 'cli-qrstuvwxyz', {expired}, 1, 1), \
             (3, 'PHID-USER-alice', 'clr', 'clr-0123456789', {later}, 1, 1), \
             (4, 'PHID-USER-bob', 'api', 'api-bobbobbob', NULL, 1, 1), \
             (5, 'PHID-USER-bot', 'api', 'api-botbotbot', NULL, 1, 1)",
            alice = session_key("alice-token"),
            admin = session_key("admin-token"),
            expires = NOW + 29 * 86_400,
            expired = NOW - 1,
            later = NOW + 3600,
        ))
        .await;
    Some(test_db)
}

fn config() -> Config {
    let mut config = Config::default();
    config.push_layer(
        "test",
        serde_json::Map::from_iter([(
            "phabricator.base-uri".to_owned(),
            json!("http://phabricator.test/"),
        )]),
    );
    config
}

async fn send(
    test_db: &TestDatabase,
    request: axum::http::request::Builder,
    body: Body,
) -> Response {
    let config = config();
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn get(test_db: &TestDatabase, path: &str, cookie: &str) -> Response {
    send(
        test_db,
        Request::get(path)
            .header(header::COOKIE, cookie)
            .header(header::HOST, "phabricator.test"),
        Body::empty(),
    )
    .await
}

/// Posts the dialog of `path` (`__form__`, `__dialog__` and the token of
/// the page) with `pairs`.
async fn post(
    test_db: &TestDatabase,
    path: &str,
    cookie: &str,
    pairs: &[(&str, &str)],
) -> Response {
    let page = text(get(test_db, path, cookie).await).await;
    let token = csrf_token(&page);
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("__csrf__", &token)
        .append_pair("__form__", "1")
        .append_pair("__dialog__", "1")
        .extend_pairs(pairs)
        .finish();
    let path = path.split('?').next().unwrap();
    send(
        test_db,
        Request::post(path)
            .header(header::COOKIE, cookie)
            .header(header::HOST, "phabricator.test")
            .header(header::USER_AGENT, "test-agent")
            .header("x-forwarded-for", "10.1.2.3")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Body::from(body),
    )
    .await
}

fn csrf_token(page: &str) -> String {
    page.split("name=\"csrf-token\" content=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("no CSRF token: {page}"))
        .to_owned()
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

fn location(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::FOUND);
    response.headers()[header::LOCATION].to_str().unwrap()
}

/// The rows `sql` selects, as one `line` column each.
async fn lines(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .iter()
        .map(|row| phab_db::rows::text(row, "line").expect("a line"))
        .collect()
}

async fn tokens(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', id, objectPHID, tokenType, LEFT(token, 4), LENGTH(token), \
           IFNULL(expires, '-'), dateCreated, dateModified) AS line \
         FROM `{ns}_conduit`.conduit_token ORDER BY id",
    )
    .await
}

async fn emails(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', userPHID, address, isVerified, isPrimary, \
           IFNULL(LENGTH(verificationCode), '-'), dateCreated, dateModified, LEFT(phid, 10)) \
           AS line FROM `{ns}_user`.user_email ORDER BY address",
    )
    .await
}

async fn user_logs(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', actorPHID, userPHID, action, oldValue, newValue, details, \
           dateCreated, remoteAddr, session = '{alice}') AS line \
         FROM `{ns}_user`.user_log ORDER BY id"
            .replace("{alice}", &session_key("alice-token"))
            .as_str(),
    )
    .await
}

/// Queued tasks: class, priority and data.
async fn tasks(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', t.taskClass, t.priority, IFNULL(t.objectPHID, '-'), d.data) \
           AS line FROM `{ns}_worker`.worker_activetask t \
         JOIN `{ns}_worker`.worker_taskdata d ON d.id = t.dataID ORDER BY t.id",
    )
    .await
}

/// Queued mail: actor, related object, the parameters and its task's
/// data, which is the mail's ID.
async fn mails(test_db: &TestDatabase) -> Vec<(String, Value)> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', IFNULL(actorPHID, '-'), relatedPHID, status, LEFT(phid, 10), \
           parameters) AS line FROM `{ns}_metamta`.metamta_mail ORDER BY id",
    )
    .await
    .into_iter()
    .map(|line| {
        let (head, parameters) = line.split_at(line.find('{').expect("JSON"));
        (
            head.trim_end().to_owned(),
            serde_json::from_str(parameters).unwrap(),
        )
    })
    .collect()
}

async fn sessions(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', id, IFNULL(highSecurityUntil, '-')) AS line \
         FROM `{ns}_user`.phabricator_session ORDER BY id",
    )
    .await
}

#[tokio::test]
async fn tokens_are_generated_shown_and_terminated() {
    let Some(test_db) = setup().await else {
        return;
    };
    let path = "/conduit/token/edit/?objectPHID=PHID-USER-alice";
    let page = text(get(&test_db, path, ALICE).await).await;
    assert!(page.contains("Generate a new API token?"), "{page}");
    assert!(page.contains(r#"name="objectPHID" value="PHID-USER-alice""#));
    assert!(page.contains(r#"href="/settings/user/alice/page/apitokens/""#));

    let response = post(&test_db, path, ALICE, &[("objectPHID", "PHID-USER-alice")]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/conduit/token/edit/6/"
    );
    let rows = tokens(&test_db).await;
    assert_eq!(
        rows[5],
        format!("6 PHID-USER-alice api api- 32 - {NOW} {NOW}")
    );

    // Its page shows it; a token which expired is not found.
    let page = text(get(&test_db, "/conduit/token/edit/6/", ALICE).await).await;
    assert!(
        page.contains("View API Token") && page.contains(r#"value="api-"#),
        "{page}"
    );
    let page = text(get(&test_db, "/conduit/token/edit/3/", ALICE).await).await;
    assert!(page.contains("automatically generated") && !page.contains("clr-0123"));
    let response = get(&test_db, "/conduit/token/edit/2/", ALICE).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Terminating one token.
    let path = "/conduit/token/terminate/1/";
    let page = text(get(&test_db, path, ALICE).await).await;
    assert!(page.contains("Really terminate this token?"), "{page}");
    let response = post(&test_db, path, ALICE, &[("objectPHID", "PHID-USER-alice")]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/settings/user/alice/page/apitokens/"
    );
    assert_eq!(
        tokens(&test_db).await[0],
        format!("1 PHID-USER-alice api api- 20 {} 1 {NOW}", NOW - 60)
    );

    // Terminating all of them, then none is left.
    let path = "/conduit/token/terminate/?objectPHID=PHID-USER-alice";
    let response = post(&test_db, path, ALICE, &[("objectPHID", "PHID-USER-alice")]).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let rows = tokens(&test_db).await;
    assert!(
        rows[2].ends_with(&format!("{} 1 {NOW}", NOW - 60)),
        "{rows:?}"
    );
    assert!(
        rows[5].contains(&format!(" {} {NOW} {NOW}", NOW - 60)),
        "{rows:?}"
    );
    assert_eq!(rows[3], "4 PHID-USER-bob api api- 13 - 1 1");
    let page = text(get(&test_db, path, ALICE).await).await;
    assert!(page.contains("No Tokens to Terminate"), "{page}");

    // Other users' tokens are PHP's; administrators edit bots' tokens.
    for path in [
        "/conduit/token/edit/?objectPHID=PHID-USER-bob",
        "/conduit/token/edit/4/",
        "/conduit/token/terminate/4/",
        "/conduit/token/terminate/?objectPHID=PHID-USER-bob",
        "/conduit/token/edit/",
        "/conduit/token/edit/?objectPHID=PHID-USER-alice&__wflow__=true",
    ] {
        let response = get(&test_db, path, ALICE).await;
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    }
    let response = get(
        &test_db,
        "/conduit/token/edit/?objectPHID=PHID-USER-list",
        ADMIN,
    )
    .await;
    assert_eq!(fallback_reason(&response), "the tokens of another user");
    let path = "/conduit/token/terminate/5/";
    let response = post(&test_db, path, ADMIN, &[("objectPHID", "PHID-USER-bot")]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/settings/user/bot/page/apitokens/"
    );
    assert!(tokens(&test_db).await[4].contains(&format!(" {} ", NOW - 60)));
    test_db.drop_databases().await;
}

#[tokio::test]
async fn token_actions_require_high_security() {
    let Some(test_db) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_auth`.auth_factorprovider (phid, providerFactorKey, status, \
               properties, dateCreated, dateModified, name) VALUES \
             ('PHID-FPRV-1', 'totp', 'active', '{}', 1, 1, ''); \
             INSERT INTO `{ns}_auth`.auth_factorconfig (phid, userPHID, factorName, \
               factorSecret, properties, dateCreated, dateModified, factorProviderPHID) VALUES \
             ('PHID-AFTR-1', 'PHID-USER-alice', 'Phone', 'GEZDGNBVGY3TQOJQ', '{}', 1, 1, \
               'PHID-FPRV-1')",
        )
        .await;
    let path = "/conduit/token/edit/?objectPHID=PHID-USER-alice";
    let response = get(&test_db, path, ALICE).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("Enter High Security"), "{page}");
    assert!(page.contains(r#"name="__hisec__""#));
    assert!(page.contains(r#"name="objectPHID" value="PHID-USER-alice""#));
    assert!(!page.contains("Generate a new API token?"));
    let challenges = lines(
        &test_db,
        "SELECT CONCAT_WS(' ', userPHID, workflowKey) AS line FROM `{ns}_auth`.auth_challenge",
    )
    .await;
    assert_eq!(challenges, ["PHID-USER-alice legacy"]);

    // A post without the prompt's answer generates nothing.
    let response = post(&test_db, path, ALICE, &[("objectPHID", "PHID-USER-alice")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(tokens(&test_db).await.len(), 5);

    // In high security, the dialog shows.
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_user`.phabricator_session SET highSecurityUntil = {} WHERE id = 1",
            NOW + 300
        ))
        .await;
    let page = text(get(&test_db, path, ALICE).await).await;
    assert!(page.contains("Generate a new API token?"), "{page}");

    // Factors other than TOTP are PHP's.
    test_db
        .execute(
            "UPDATE `{ns}_user`.phabricator_session SET highSecurityUntil = NULL; \
             UPDATE `{ns}_auth`.auth_factorprovider SET providerFactorKey = 'sms'",
        )
        .await;
    let response = get(&test_db, path, ALICE).await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn sessions_are_terminated_and_leave_high_security() {
    let Some(test_db) = setup().await else {
        return;
    };
    let page = text(get(&test_db, "/auth/session/terminate/3/", ALICE).await).await;
    assert!(
        page.contains("Really terminate session <strong>abcdef</strong>?"),
        "{page}"
    );
    let response = post(&test_db, "/auth/session/terminate/3/", ALICE, &[]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/settings/panel/sessions/"
    );
    assert_eq!(sessions(&test_db).await, ["1 -", "2 -", "4 -"]);

    // The current session is never terminated; nor are other users'.
    for path in ["/auth/session/terminate/1/", "/auth/session/terminate/2/"] {
        let page = text(get(&test_db, path, ALICE).await).await;
        assert!(page.contains("No Matching Sessions"), "{page}");
    }
    // A post without `__dialog__` only shows the dialog.
    let page = text(get(&test_db, "/auth/session/terminate/all/", ALICE).await).await;
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("__csrf__", &csrf_token(&page))
        .append_pair("__form__", "1")
        .finish();
    let response = send(
        &test_db,
        Request::post("/auth/session/terminate/all/")
            .header(header::COOKIE, ALICE)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Body::from(body),
    )
    .await;
    assert!(
        text(response)
            .await
            .contains("Really terminate all sessions?")
    );
    // A bad CSRF token is PHP's.
    let response = send(
        &test_db,
        Request::post("/auth/session/terminate/all/")
            .header(header::COOKIE, ALICE)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Body::from("__csrf__=nope&__form__=1&__dialog__=1"),
    )
    .await;
    assert_eq!(fallback_reason(&response), "an invalid CSRF token");
    let response = post(&test_db, "/auth/session/terminate/all/", ALICE, &[]).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(sessions(&test_db).await, ["1 -", "2 -"]);

    // Leaving high security.
    let page = text(get(&test_db, "/auth/session/downgrade/", ALICE).await).await;
    assert!(page.contains("Normal Security Restored"), "{page}");
    test_db
        .execute(&format!(
            "UPDATE `{{ns}}_user`.phabricator_session SET highSecurityUntil = {} WHERE id = 1",
            NOW + 300
        ))
        .await;
    let response = post(&test_db, "/auth/session/downgrade/", ALICE, &[]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/auth/session/downgrade/"
    );
    assert_eq!(sessions(&test_db).await, ["1 -", "2 -"]);
    assert_eq!(
        user_logs(&test_db).await,
        [format!(
            "PHID-USER-alice PHID-USER-alice hisec-exit null null \
             {{\"host\":\"rust-host\",\"user_agent\":\"test-agent\"}} {NOW} 10.1.2.3 1"
        )]
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn addresses_are_added_with_a_verification_mail() {
    let Some(test_db) = setup().await else {
        return;
    };
    let path = "/settings/user/alice/page/email/?new=true";
    let page = text(get(&test_db, path, ALICE).await).await;
    assert!(
        page.contains("New Address") && page.contains(r#"name="email""#),
        "{page}"
    );

    let pairs = [("new", "true"), ("email", " new@example.com ")];
    let response = post(&test_db, path, ALICE, &pairs).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("Verification Email Sent"), "{page}");
    assert!(page.contains(r#"name="new" value="verify""#));
    let rows = emails(&test_db).await;
    assert_eq!(
        rows[4],
        format!("PHID-USER-alice new@example.com 0 0 24 {NOW} {NOW} PHID-EADR-")
    );
    assert_eq!(
        user_logs(&test_db).await,
        [format!(
            "PHID-USER-alice PHID-USER-alice email-add null \"new@example.com\" \
             {{\"host\":\"rust-host\",\"user_agent\":\"test-agent\"}} {NOW} 10.1.2.3 1"
        )]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', actorIdentity, action, score, epoch) AS line \
             FROM `{ns}_system`.system_actionlog",
        )
        .await,
        [format!("PHID-USER-alice email.add 1 {NOW}")]
    );
    let code = lines(
        &test_db,
        "SELECT verificationCode AS line FROM `{ns}_user`.user_email \
         WHERE address = 'new@example.com'",
    )
    .await
    .remove(0);
    let mails = mails(&test_db).await;
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].0, "- PHID-USER-alice queued PHID-MTAM-");
    assert_eq!(
        mails[0].1,
        json!({
            "sensitive": true,
            "mustEncrypt": false,
            "raw-to": ["new@example.com"],
            "force": true,
            "subject": "[Phabricator] Email Verification",
            "body": format!(
                "Hi alice\n\nPlease verify that you own this email address (new@example.com) \
                 by clicking this link:\n\n  http://phabricator.test/emailverify/{code}/\n\n\
                 Get Well Soon,\nPhabricator"
            ),
        })
    );
    let tasks = tasks(&test_db).await;
    assert_eq!(
        tasks[0],
        "PhabricatorRepositoryIdentityChangeWorker 2000 - {\"emailAddresses\":[\"new@example.com\"]}"
    );
    assert!(
        tasks[1].starts_with("PhabricatorMetaMTAWorker 1000 - "),
        "{tasks:?}"
    );

    // "Done" goes back to the panel.
    let response = post(&test_db, path, ALICE, &[("new", "verify")]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/settings/user/alice/page/email/"
    );

    // Addresses PHP refuses or another account has are PHP's.
    for address in ["bob@example.com", "not an address", ""] {
        let pairs = [("new", "true"), ("email", address)];
        let response = post(&test_db, path, ALICE, &pairs).await;
        assert_eq!(
            response.status(),
            StatusCode::MISDIRECTED_REQUEST,
            "{address}"
        );
    }
    assert_eq!(emails(&test_db).await.len(), 5);
    test_db.drop_databases().await;
}

#[tokio::test]
async fn addresses_are_removed_verified_and_made_primary() {
    let Some(test_db) = setup().await else {
        return;
    };
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_auth`.auth_temporarytoken (tokenResource, tokenType, \
               tokenExpires, tokenCode, userPHID, properties) VALUES \
             ('PHID-USER-alice', 'login:password', {later}, 'x', 'PHID-USER-alice', '{{}}'), \
             ('PHID-USER-alice', 'login:other', {later}, 'y', 'PHID-USER-alice', '{{}}')",
            later = NOW + 3600,
        ))
        .await;
    let base = "/settings/user/alice/page/email/";

    // Removing an address.
    let page = text(get(&test_db, &format!("{base}?delete=1"), ALICE).await).await;
    assert!(
        page.contains("Really delete address") && page.contains("b@example.com"),
        "{page}"
    );
    let response = post(
        &test_db,
        &format!("{base}?delete=1"),
        ALICE,
        &[("delete", "1")],
    )
    .await;
    assert_eq!(
        location(&response),
        format!("http://phabricator.test{base}")
    );
    assert_eq!(emails(&test_db).await.len(), 3);
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', objectClass, IFNULL(rootLogID, '-'), objectPHID, \
               IFNULL(objectMonogram, '-'), epoch) AS line \
             FROM `{ns}_system`.system_destructionlog",
        )
        .await,
        [format!("PhabricatorUserEmail - PHID-EADR-1 - {NOW}")]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', tokenType, tokenExpires) AS line \
             FROM `{ns}_auth`.auth_temporarytoken ORDER BY id",
        )
        .await,
        [
            format!("login:password {}", NOW - 1),
            format!("login:other {}", NOW + 3600)
        ]
    );
    // The primary address and other users' are not found.
    for path in [format!("{base}?delete=2"), format!("{base}?delete=4")] {
        let response = get(&test_db, &path, ALICE).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }

    // Making an address primary.
    let page = text(get(&test_db, &format!("{base}?primary=3"), ALICE).await).await;
    assert!(
        page.contains("Phabricator will send all email to c@example.com."),
        "{page}"
    );
    let response = post(
        &test_db,
        &format!("{base}?primary=3"),
        ALICE,
        &[("primary", "3")],
    )
    .await;
    assert_eq!(
        location(&response),
        format!("http://phabricator.test{base}")
    );
    let rows = emails(&test_db).await;
    assert_eq!(
        rows[0],
        format!("PHID-USER-alice a@example.com 1 0 5 1 {NOW} PHID-EADR-")
    );
    // An address without a code gets one, as `save()` does.
    assert_eq!(
        rows[2],
        format!("PHID-USER-alice c@example.com 1 1 24 1 {NOW} PHID-EADR-")
    );
    let logs = user_logs(&test_db).await;
    assert_eq!(logs.len(), 2);
    assert!(
        logs[0].starts_with("PHID-USER-alice PHID-USER-alice email-remove \"b@example.com\" null"),
        "{logs:?}"
    );
    assert!(
        logs[1].starts_with(
            "PHID-USER-alice PHID-USER-alice email-primary \"a@example.com\" \"c@example.com\""
        ),
        "{logs:?}"
    );
    let mails = mails(&test_db).await;
    assert_eq!(mails.len(), 2);
    assert_eq!(
        mails[0].0,
        "PHID-USER-alice PHID-USER-alice queued PHID-MTAM-"
    );
    assert_eq!(mails[0].1["raw-to"], json!(["a@example.com"]));
    assert_eq!(mails[0].1["from"], json!("PHID-USER-alice"));
    assert_eq!(
        mails[0].1["body"],
        json!(
            "Hi alice\n\nThis email address (a@example.com) is no longer your primary email \
             address. Going forward, all email will be sent to your new primary email address \
             (c@example.com).\n"
        )
    );
    assert_eq!(mails[1].1["raw-to"], json!(["c@example.com"]));
    assert_eq!(
        mails[1].1["subject"],
        json!("[Phabricator] Primary Address Changed")
    );
    let tasks = tasks(&test_db).await;
    assert_eq!(
        tasks[0],
        "PhabricatorRepositoryIdentityChangeWorker 2000 - \
         {\"relatedPHIDs\":[],\"emailAddresses\":[\"b@example.com\"]}"
    );
    assert_eq!(tasks.len(), 3);

    // Sending another verification mail: no address is unverified now.
    let response = get(&test_db, &format!("{base}?verify=1"), ALICE).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user_email (id, phid, userPHID, address, isVerified, \
               isPrimary, verificationCode, dateCreated, dateModified) VALUES \
             (9, 'PHID-EADR-9', 'PHID-USER-alice', 'd@example.com', 0, 0, 'coded', 1, 1)",
        )
        .await;
    let page = text(get(&test_db, &format!("{base}?verify=9"), ALICE).await).await;
    assert!(
        page.contains("Send another copy of the verification email to d@example.com?"),
        "{page}"
    );
    let response = post(
        &test_db,
        &format!("{base}?verify=9"),
        ALICE,
        &[("verify", "9")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let mails = self::mails(&test_db).await;
    assert_eq!(mails.len(), 3);
    assert!(
        mails[2].1["body"]
            .as_str()
            .unwrap()
            .contains("http://phabricator.test/emailverify/coded/"),
        "{mails:?}"
    );

    // A removal the destruction engine would write more for is PHP's.
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-EADR-9', 1, 'PHID-X', 1, 0)",
        )
        .await;
    let response = post(
        &test_db,
        &format!("{base}?delete=9"),
        ALICE,
        &[("delete", "9")],
    )
    .await;
    assert_eq!(
        fallback_reason(&response),
        "an address other objects refer to"
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn the_external_editor_panel_saves_valid_patterns() {
    let Some(test_db) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user_preferences (phid, userPHID, preferences, \
               dateCreated, dateModified, builtinKey) VALUES \
             ('PHID-PSET-1', 'PHID-USER-alice', '{\"editor\":\"ftp://x/%f\"}', 1, 1, NULL)",
        )
        .await;
    let path = "/settings/user/alice/page/editor/";
    let page = text(get(&test_db, path, ALICE).await).await;
    assert!(page.contains("External Editor URI Variables"), "{page}");
    assert!(page.contains("<td>vscode://</td>"));
    assert!(page.contains("is not allowed."), "{page}");
    let pairs = [("editEngine", "true"), ("editor", "vscode://file/%f:%l")];
    let response = post(&test_db, path, ALICE, &pairs).await;
    assert_eq!(
        location(&response),
        format!("http://phabricator.test{path}")
    );
    let stored = lines(
        &test_db,
        "SELECT preferences AS line FROM `{ns}_user`.user_preferences",
    )
    .await;
    assert_eq!(stored, [r#"{"editor":"vscode:\/\/file\/%f:%l"}"#]);
    let pairs = [("editEngine", "true"), ("editor", "javascript:alert(1)")];
    let response = post(&test_db, path, ALICE, &pairs).await;
    assert_eq!(fallback_reason(&response), "settings PHP refuses");
    test_db.drop_databases().await;
}
