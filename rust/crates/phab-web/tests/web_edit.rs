//! The comment form and the subscribe action of revisions against a test
//! database: the rows the Rust editor writes for them, and what it leaves
//! to PHP.

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
use serde_json::{Map, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
/// A user's `accountSecret`, which fills its `binary(64)` column.
const ACCOUNT_SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
const BOB: &str = "phsid=bob-token; phab_ui=rust";
const DAVE: &str = "phsid=dave-token; phab_ui=rust";
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;

const DATABASES: &[&str] = &[
    "differential",
    "audit",
    "repository",
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "herald",
    "worker",
    "harbormaster",
    "draft",
    "search",
    "file",
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

/// Alice's revision D1, which Bob reviews and Carol resigned from, with
/// Bob's draft of a comment; Dave has a session too.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, \
               isApproved, accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, '{secret}', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, '{secret}', 0, 0), \
             ('PHID-USER-dave', 'dave', 'Dave', 1, 1, 'c', 0, 0, 0, 1, 1, '{secret}', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial, \
                highSecurityUntil) VALUES \
             ('PHID-USER-bob', 'web', '{bob}', 1699000000, {expires}, 'PHID-SSSN-1', 0, NULL), \
             ('PHID-USER-dave', 'web', '{dave}', 1699000000, {expires}, 'PHID-SSSN-2', 0, NULL); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-1'), \
             (2, 'Draft', 'PHID-DREV-2', 'draft', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, \
               reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-bob', 'added', 1, 1, '{{}}'), \
             ('PHID-DREV-1', 'PHID-USER-carol', 'resigned', 1, 1, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 64, 'PHID-USER-bob', 1, 0), \
             ('PHID-DREV-1', 64, 'PHID-USER-alice', 1, 0); \
             INSERT INTO `{{ns}}_draft`.draft_versioneddraft (objectPHID, authorPHID, version, \
               properties, dateCreated, dateModified) VALUES \
             ('PHID-DREV-1', 'PHID-USER-bob', 1, '{{\"comment\":\"LG\"}}', 1, 1), \
             ('PHID-DREV-1', 'PHID-USER-alice', 1, '{{\"comment\":\"Hm\"}}', 1, 1)",
            secret = ACCOUNT_SECRET,
            bob = session_key("bob-token"),
            dave = session_key("dave-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    Some(test_db)
}

/// A CSRF token of `user`'s session `token`.
async fn csrf_token(test_db: &TestDatabase, user: &str, token: &str) -> String {
    let Outcome::Done(key) = csrf::load_key(test_db.database()).await.unwrap() else {
        panic!("no CSRF key");
    };
    let secret = CsrfSecret::for_session(&Phid::new(user), ACCOUNT_SECRET, &session_key(token));
    csrf::new_token(&secret, &key, NOW).unwrap()
}

async fn post(
    test_db: &TestDatabase,
    path: &str,
    cookie: &str,
    pairs: &[(&str, &str)],
) -> Response {
    let config = config();
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(
            Request::post(path)
                .header(header::COOKIE, cookie)
                .header(header::HOST, "phabricator.test")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Posts the comment form of D1 as Bob.
async fn comment(test_db: &TestDatabase, actions: &str, extra: &[(&str, &str)]) -> Response {
    let token = csrf_token(test_db, "PHID-USER-bob", "bob-token").await;
    let mut pairs = vec![
        ("__form__", "1"),
        ("__csrf__", token.as_str()),
        ("__draft__", ""),
        ("draft.version", "1"),
        ("editengine.actions", actions),
        ("comment", "Looks good."),
        ("comment_metadata", "{}"),
    ];
    pairs.extend_from_slice(extra);
    post(
        test_db,
        "/differential/revision/edit/1/comment/",
        BOB,
        &pairs,
    )
    .await
}

fn fallback_reason(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    response.headers()[FALLBACK_HEADER].to_str().unwrap()
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

/// The transactions of D1: type, content source and metadata without the
/// group ID.
async fn transactions(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', transactionType, contentSource, \
           REPLACE(metadata, SUBSTRING(metadata, LOCATE('core.groupID', metadata) + 15, 32), \
             'G')) AS line \
         FROM `{ns}_differential`.differential_transaction ORDER BY id",
    )
    .await
}

async fn via_stamp(test_db: &TestDatabase) -> serde_json::Value {
    let data = lines(
        test_db,
        "SELECT data AS line FROM `{ns}_worker`.worker_taskdata ORDER BY id",
    )
    .await;
    let data: serde_json::Value = serde_json::from_str(&data[0]).unwrap();
    data["state"]["mailStamps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stamp| stamp["key"] == "via")
        .unwrap()["value"]
        .clone()
}

#[tokio::test]
async fn comments_are_written_from_the_web() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = comment(
        &test_db,
        r#"[{"type":"accept","value":true,"initialValue":null}]"#,
        &[],
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FOUND,
        "{:?}",
        response.headers()
    );
    assert_eq!(
        response.headers()[header::LOCATION],
        "http://phabricator.test/D1"
    );
    assert_eq!(
        transactions(&test_db).await,
        vec![
            format!(r#"differential.revision.accept {WEB_SOURCE} {{"core.groupID":"G"}}"#),
            format!(r#"core:comment {WEB_SOURCE} {{"remarkup.control":[],"core.groupID":"G"}}"#),
            format!("differential.revision.status {WEB_SOURCE} []"),
        ]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', content, contentSource) AS line \
             FROM `{ns}_differential`.differential_transaction_comment"
        )
        .await,
        vec![format!("Looks good. {WEB_SOURCE}")]
    );
    assert_eq!(via_stamp(&test_db).await, json!("web"));
    // Bob's draft is gone, Alice's stays.
    assert_eq!(
        lines(
            &test_db,
            "SELECT authorPHID AS line FROM `{ns}_draft`.draft_versioneddraft"
        )
        .await,
        vec!["PHID-USER-alice"]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT dst AS line FROM `{ns}_differential`.edge WHERE type = 64"
        )
        .await,
        vec!["PHID-USER-alice"]
    );
}

#[tokio::test]
async fn changing_subscribers_adds_them() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = comment(
        &test_db,
        r#"[{"type":"subscriberPHIDs","value":["PHID-USER-dave"],"initialValue":[]}]"#,
        &[("comment", "")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        transactions(&test_db).await,
        vec![format!(
            r#"core:subscribers {WEB_SOURCE} {{"core.groupID":"G"}}"#
        )]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT dst AS line FROM `{ns}_differential`.edge WHERE type = 21"
        )
        .await,
        vec!["PHID-USER-dave"]
    );
}

#[tokio::test]
async fn what_rust_does_not_write_goes_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let accept = r#"[{"type":"accept","value":true,"initialValue":null}]"#;
    for (extra, reason) in [
        (vec![("__ajax__", "true")], "a workflow request"),
        (vec![("__preview__", "1")], "previews and confirmations"),
        (vec![("__csrf__", "B@nope")], "invalid CSRF tokens"),
        (
            vec![(
                "comment_metadata",
                r#"{"attachedFilePHIDs":["PHID-FILE-1"]}"#,
            )],
            "comments with attached files or unusual metadata",
        ),
    ] {
        let response = comment(&test_db, accept, &extra).await;
        assert_eq!(fallback_reason(&response), reason, "{extra:?}");
    }
    let commandeer = r#"[{"type":"commandeer","value":true,"initialValue":null}]"#;
    let response = comment(&test_db, commandeer, &[]).await;
    assert_eq!(fallback_reason(&response), "comment actions not ported");
    // Bob may not accept for a project.
    let accept_for = r#"[{"type":"accept","value":["PHID-PROJ-1"],"initialValue":null}]"#;
    let response = comment(&test_db, accept_for, &[]).await;
    assert_eq!(fallback_reason(&response), "actions with options");

    // An unpublished inline comment of Bob's is published with the comment.
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_transaction_comment (phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, revisionPHID, changesetID, \
               isNewFile, lineNumber, lineLength, fixedState, hasReplies, replyToCommentPHID, \
               attributes) \
             VALUES ('PHID-XCMT-1', NULL, 'PHID-USER-bob', 'public', 'PHID-USER-bob', 1, 'Hm', \
               '{}', 0, 1, 1, 'PHID-DREV-1', 1, 1, 1, 1, NULL, 0, NULL, '[]')",
        )
        .await;
    let response = comment(&test_db, accept, &[]).await;
    assert_eq!(fallback_reason(&response), "inline comments to publish");
    assert_eq!(transactions(&test_db).await, Vec::<String>::new());
}

#[tokio::test]
async fn subscribing_writes_a_web_transaction() {
    let Some(test_db) = setup().await else {
        return;
    };
    let token = csrf_token(&test_db, "PHID-USER-dave", "dave-token").await;
    let form = [("__form__", "1"), ("__csrf__", token.as_str())];
    let response = post(&test_db, "/subscriptions/add/PHID-DREV-1/", DAVE, &form).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response.headers()[header::LOCATION],
        "http://phabricator.test/D1"
    );
    assert_eq!(
        transactions(&test_db).await,
        vec![format!(
            r#"core:subscribers {WEB_SOURCE} {{"core.groupID":"G"}}"#
        )]
    );
    assert_eq!(via_stamp(&test_db).await, json!("web"));
    // Drafts are the comment form's.
    assert_eq!(
        lines(
            &test_db,
            "SELECT CAST(COUNT(*) AS CHAR) AS line FROM `{ns}_draft`.draft_versioneddraft"
        )
        .await,
        vec!["2"]
    );

    // Bob reviews D1: he's subscribed already, as PHP says in a dialog.
    let token = csrf_token(&test_db, "PHID-USER-bob", "bob-token").await;
    let form = [("__form__", "1"), ("__csrf__", token.as_str())];
    let response = post(&test_db, "/subscriptions/add/PHID-DREV-1/", BOB, &form).await;
    assert_eq!(fallback_reason(&response), "automatic subscriptions");
    let response = post(&test_db, "/subscriptions/add/PHID-TASK-1/", BOB, &form).await;
    assert_eq!(fallback_reason(&response), "subscriptions to other objects");
}

#[tokio::test]
async fn drafts_go_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let token = csrf_token(&test_db, "PHID-USER-bob", "bob-token").await;
    let response = post(
        &test_db,
        "/differential/revision/edit/2/comment/",
        BOB,
        &[
            ("__form__", "1"),
            ("__csrf__", token.as_str()),
            ("comment", "Early look."),
        ],
    )
    .await;
    assert_eq!(fallback_reason(&response), "comments on drafts");
}
