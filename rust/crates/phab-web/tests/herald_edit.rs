//! The Herald writes of the web interface against a test database:
//! disabling and enabling rules, the rule editor, and the test console.
//!
//! Root is an administrator, who may edit global rules; Alice wrote the
//! personal rule H2. H1 is a global revision rule.

// Few databases, many scenarios each: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use phab_auth::Outcome;
use phab_auth::csrf::{self, CsrfSecret};
use phab_core::{Config, Phid, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::{FALLBACK_HEADER, WebState};
use serde_json::{Map, Value, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
/// A user's `accountSecret`, which fills its `binary(64)` column.
const ACCOUNT_SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;

const DATABASES: &[&str] = &[
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "herald",
    "worker",
    "search",
    "project",
    "differential",
    "repository",
    "file",
    "harbormaster",
];

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
        Some("web1")
    }
}

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap()
}

fn config() -> Config {
    let mut config = Config::default();
    let mut values = Map::new();
    values.insert(
        "phabricator.base-uri".to_owned(),
        json!("http://phabricator.test/"),
    );
    config.push_layer("test", values);
    config
}

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (1, 'PHID-USER-root', 'root', 'Root', 1, 1, 'c', 0, 0, 1, 1, 1, '{secret}', 0, 0), \
             (2, 'PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, '{secret}', 0, \
               0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial, \
                highSecurityUntil) VALUES \
             ('PHID-USER-root', 'web', '{root}', 1699000000, {expires}, 'PHID-SSSN-1', 0, NULL), \
             ('PHID-USER-alice', 'web', '{alice}', 1699000000, {expires}, 'PHID-SSSN-2', 0, \
               NULL); \
             INSERT INTO `{{ns}}_herald`.herald_rule (id, name, authorPHID, contentType, \
               mustMatchAll, configVersion, dateCreated, dateModified, repetitionPolicy, ruleType, \
               phid, isDisabled, triggerObjectPHID) VALUES \
             (1, 'Tag CI', 'PHID-USER-root', 'differential', 1, 38, 1, 1, 'every', 'global', \
               'PHID-HRUL-1', 0, NULL), \
             (2, 'Mine', 'PHID-USER-alice', 'differential', 0, 37, 1, 1, 'first', 'personal', \
               'PHID-HRUL-2', 1, NULL); \
             INSERT INTO `{{ns}}_herald`.herald_condition (id, ruleID, fieldName, fieldCondition, \
               value) VALUES \
             (1, 1, 'differential.revision.title', 'contains', '\"ci\"'), \
             (2, 2, 'cc', 'exists', 'null'); \
             INSERT INTO `{{ns}}_herald`.herald_action (id, ruleID, action, target) VALUES \
             (1, 1, 'projects.add', '[\"PHID-PROJ-ci\"]'), \
             (2, 2, 'email.self', 'null'); \
             INSERT INTO `{{ns}}_herald`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-HRUL-1', 21, 'PHID-USER-alice', 1, 0); \
             INSERT INTO `{{ns}}_herald`.herald_savedheader (phid, header) VALUES \
             ('PHID-HRUL-1', '<3>')",
            secret = ACCOUNT_SECRET,
            root = session_key("root-token"),
            alice = session_key("alice-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    Some(test_db)
}

async fn csrf_token(test_db: &TestDatabase, user: &str) -> String {
    let Outcome::Done(key) = csrf::load_key(test_db.database()).await.unwrap() else {
        panic!("no CSRF key");
    };
    let secret = CsrfSecret::for_session(
        &Phid::new(format!("PHID-USER-{user}")),
        ACCOUNT_SECRET,
        &session_key(&format!("{user}-token")),
    );
    csrf::new_token(&secret, &key, NOW).unwrap()
}

async fn send(test_db: &TestDatabase, request: Request<Body>) -> Response {
    let config = config();
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request)
        .await
        .unwrap()
}

/// Posts a form as `user`, with `__form__` and a valid CSRF token.
async fn post(test_db: &TestDatabase, user: &str, path: &str, pairs: &[(&str, &str)]) -> Response {
    let token = csrf_token(test_db, user).await;
    let mut all = vec![("__form__", "1"), ("__csrf__", token.as_str())];
    all.extend_from_slice(pairs);
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(&all)
        .finish();
    let request = Request::post(path)
        .header(header::COOKIE, format!("phsid={user}-token; phab_ui=rust"))
        .header(header::HOST, "phabricator.test")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(test_db, request).await
}

/// Gets a page as `user`: its status and body.
async fn page(test_db: &TestDatabase, user: &str, path: &str) -> (StatusCode, String) {
    let request = Request::get(path)
        .header(header::COOKIE, format!("phsid={user}-token; phab_ui=rust"))
        .header(header::HOST, "phabricator.test")
        .body(Body::empty())
        .unwrap();
    let response = send(test_db, request).await;
    let status = response.status();
    let reason = response
        .headers()
        .get(FALLBACK_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body).into_owned();
    (
        status,
        reason.map_or(body, |reason| format!("fallback: {reason}")),
    )
}

fn fallback_reason(response: &Response) -> &str {
    assert_eq!(
        response.status(),
        StatusCode::MISDIRECTED_REQUEST,
        "{response:?}"
    );
    response.headers()[FALLBACK_HEADER].to_str().unwrap()
}

fn location(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::FOUND, "{response:?}");
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

/// The rule transactions: object, type, values, author, content source,
/// metadata without the group.
async fn transactions(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', objectPHID, transactionType, oldValue, newValue, authorPHID, \
           editPolicy, contentSource, REPLACE(metadata, SUBSTRING(metadata, \
           LOCATE('core.groupID', metadata) + 15, 32), 'G')) AS line \
         FROM `{ns}_herald`.herald_ruletransaction ORDER BY id",
    )
    .await
}

/// The state of the last publish task.
async fn last_task(test_db: &TestDatabase) -> Value {
    let data = lines(
        test_db,
        "SELECT CAST(d.data AS CHAR) AS line FROM `{ns}_worker`.worker_activetask t \
         JOIN `{ns}_worker`.worker_taskdata d ON d.id = t.dataID \
         WHERE t.taskClass = 'PhabricatorApplicationTransactionPublishWorker' \
         ORDER BY t.id DESC LIMIT 1",
    )
    .await;
    serde_json::from_str(&data[0]).unwrap()
}

#[tokio::test]
async fn rules_are_disabled_and_enabled_from_the_web() {
    let Some(test_db) = setup().await else {
        return;
    };
    // Only administrators may edit global rules.
    let response = post(&test_db, "alice", "/herald/disable/1/disable/", &[]).await;
    assert_eq!(fallback_reason(&response), "policy exceptions");
    // Without a CSRF token, PHP explains.
    let request = Request::post("/herald/disable/1/disable/")
        .header(header::COOKIE, "phsid=root-token; phab_ui=rust")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("__form__=1"))
        .unwrap();
    assert_eq!(
        fallback_reason(&send(&test_db, request).await),
        "invalid CSRF tokens"
    );

    let response = post(&test_db, "root", "/herald/disable/1/disable/", &[]).await;
    assert_eq!(location(&response), "http://phabricator.test/H1");
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', name, isDisabled, configVersion, dateModified) AS line \
             FROM `{ns}_herald`.herald_rule WHERE id = 1",
        )
        .await,
        [format!("Tag CI 1 38 {NOW}")]
    );
    assert_eq!(
        transactions(&test_db).await,
        [format!(
            "PHID-HRUL-1 herald:disable false true PHID-USER-root PHID-USER-root {WEB_SOURCE} \
             {{\"core.groupID\":\"G\"}}"
        )]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', objectPHID, host, dryRun, time) AS line \
             FROM `{ns}_herald`.herald_transcript",
        )
        .await,
        [format!("PHID-HRUL-1 web1 0 {NOW}")]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', phid, header) AS line FROM `{ns}_herald`.herald_savedheader",
        )
        .await,
        ["PHID-HRUL-1 <3>, none"]
    );
    let task = last_task(&test_db).await;
    let state = &task["state"];
    assert_eq!(task["objectPHID"], "PHID-HRUL-1");
    assert_eq!(task["actorPHID"], "PHID-USER-root");
    assert_eq!(state["isNewObject"], false);
    assert_eq!(state["heraldHeader"], "<3>, none");
    assert_eq!(state["mailToPHIDs"], json!(["PHID-USER-root"]));
    assert_eq!(state["mailCCPHIDs"], json!(["PHID-USER-alice"]));
    assert_eq!(state["mailShouldSend"], true);
    assert_eq!(state["feedShouldPublish"], false);
    let stamps: Vec<&str> = state["mailStamps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stamp| stamp["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        stamps,
        [
            "application",
            "monogram",
            "phid",
            "object-type",
            "actor",
            "via",
            "silent",
            "encrypted",
            "new",
            "mention",
            "herald",
            "removed",
            "subscriber"
        ]
    );
    assert_eq!(state["mailStamps"][10]["value"], json!(["H3"]));

    // Disabled already: PHP saves and publishes it anyway.
    let response = post(&test_db, "root", "/herald/disable/1/disable/", &[]).await;
    assert_eq!(fallback_reason(&response), "edits without effect");

    // Alice enables her personal rule; she gets the mail twice over.
    let response = post(&test_db, "alice", "/herald/disable/2/enable/", &[]).await;
    assert_eq!(location(&response), "http://phabricator.test/H2");
    let task = last_task(&test_db).await;
    assert_eq!(
        task["state"]["mailToPHIDs"],
        json!(["PHID-USER-alice", "PHID-USER-alice"])
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', isDisabled, configVersion) AS line \
             FROM `{ns}_herald`.herald_rule WHERE id = 2",
        )
        .await,
        ["0 37"]
    );

    // Rules about Herald rules run on these edits.
    test_db
        .execute(
            "INSERT INTO `{ns}_herald`.herald_rule (id, name, authorPHID, contentType, \
               mustMatchAll, configVersion, dateCreated, dateModified, repetitionPolicy, \
               ruleType, phid, isDisabled, triggerObjectPHID) VALUES \
             (9, 'Watch rules', 'PHID-USER-root', 'HeraldRuleAdapter', 1, 38, 1, 1, 'every', \
               'global', 'PHID-HRUL-9', 0, NULL)",
        )
        .await;
    let response = post(&test_db, "root", "/herald/disable/1/enable/", &[]).await;
    assert_eq!(
        fallback_reason(&response),
        "Herald rules about Herald rules"
    );
    test_db
        .execute("DELETE FROM `{ns}_herald`.herald_rule WHERE id = 9")
        .await;

    let (status, body) = page(&test_db, "root", "/herald/disable/1/enable/").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Really enable this rule?"), "{body}");
    test_db.drop_databases().await;
}
