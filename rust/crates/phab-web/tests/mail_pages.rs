//! The pages mail and menus link to: change details and comment
//! histories of revision transactions, subscribers of revisions, and the
//! viewer's flags.

// The checks share one database, so they are one long function.
#![allow(clippy::too_many_lines)]

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

const ALICE: &str = "phsid=alice-token";
const BOB: &str = "phsid=bob-token";

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
}

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap()
}

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "user",
        "auth",
        "differential",
        "project",
        "policy",
        "spaces",
        "repository",
        "file",
        "harbormaster",
        "flag",
        "search",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0), \
             ('PHID-USER-bob', 'web', '{bob}', 1, {expires}, 'PHID-SSSN-2', 0); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', '{{}}', 'PHID-DIFF-1'), \
             (2, 'Hidden', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'PHID-USER-alice', 'users', '{{}}', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_transaction (id, phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, \
               transactionType, oldValue, newValue, contentSource, metadata, dateCreated, \
               dateModified) VALUES \
             (1, 'PHID-XACT-DREV-sum', 'PHID-USER-alice', 'PHID-DREV-1', 'users', 'users', NULL, \
               0, 'differential.revision.summary', '\"Old <summary>\"', '\"New summary\"', \
               '{{}}', '{{}}', 10, 10), \
             (2, 'PHID-XACT-DREV-com', 'PHID-USER-alice', 'PHID-DREV-1', 'users', 'users', \
               'PHID-XCMT-2', 2, 'core:comment', 'null', 'null', '{{}}', '{{}}', 20, 30), \
             (3, 'PHID-XACT-DREV-hid', 'PHID-USER-alice', 'PHID-DREV-2', 'users', 'users', NULL, \
               0, 'differential.revision.summary', '\"a\"', '\"b\"', '{{}}', '{{}}', 10, 10); \
             INSERT INTO `{{ns}}_differential`.differential_transaction_comment (phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, isNewFile, lineNumber, \
               lineLength, hasReplies, attributes) VALUES \
             ('PHID-XCMT-1', 'PHID-XACT-DREV-com', 'PHID-USER-alice', 'users', 'users', 1, \
               'First **draft**', '{{}}', 0, 20, 20, 0, 0, 0, 0, '{{}}'), \
             ('PHID-XCMT-2', 'PHID-XACT-DREV-com', 'PHID-USER-alice', 'users', 'users', 2, \
               'Second take', '{{}}', 0, 30, 30, 0, 0, 0, 0, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 21, 'PHID-USER-bob', 1, 0), ('PHID-DREV-1', 21, 'PHID-USER-alice', 2, 0); \
             INSERT INTO `{{ns}}_flag`.flag (ownerPHID, type, objectPHID, reasonPHID, color, \
               note, dateCreated, dateModified) VALUES \
             ('PHID-USER-alice', 'DREV', 'PHID-DREV-1', 'PHID-USER-alice', 3, 'Look again', 5, 5), \
             ('PHID-USER-bob', 'DREV', 'PHID-DREV-1', 'PHID-USER-bob', 0, 'Bob''s', 5, 5)",
            alice = session_key("alice-token"),
            bob = session_key("bob-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    Some(test_db)
}

async fn get(test_db: &TestDatabase, path: &str, cookie: &str) -> Response {
    let mut config = Config::default();
    if let serde_json::Value::Object(map) = json!({"phabricator.base-uri": "http://phab.test/"}) {
        config.push_layer("test", map);
    }
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(
            Request::get(path)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn page(test_db: &TestDatabase, path: &str, cookie: &str) -> String {
    let response = get(test_db, path, cookie).await;
    let status = response.status();
    let reason = response
        .headers()
        .get(FALLBACK_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(status, StatusCode::OK, "{path}: {reason:?} {text}");
    text
}

async fn fallback(test_db: &TestDatabase, path: &str, cookie: &str) -> String {
    let response = get(test_db, path, cookie).await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    response.headers()[FALLBACK_HEADER]
        .to_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn pages_linked_from_mail() {
    let Some(test_db) = setup().await else {
        return;
    };

    let detail = page(&test_db, "/transactions/detail/PHID-XACT-DREV-sum/", BOB).await;
    assert!(detail.contains("<a href=\"/D1\">D1: Open</a>"), "{detail}");
    assert!(detail.contains("Old &#60;summary&#62;</pre>"), "{detail}");
    assert!(detail.contains("New summary</pre>"), "{detail}");
    assert_eq!(
        fallback(
            &test_db,
            "/transactions/detail/PHID-XACT-DREV-sum/?__ajax__=1",
            BOB
        )
        .await,
        "a transaction dialog"
    );
    assert_eq!(
        fallback(&test_db, "/transactions/detail/PHID-XACT-DREV-com/", BOB).await,
        "change details not ported"
    );
    assert_eq!(
        fallback(&test_db, "/transactions/detail/PHID-XACT-TASK-1/", BOB).await,
        "a transaction of an object not ported"
    );
    // Only Alice sees D2.
    assert_eq!(
        get(&test_db, "/transactions/detail/PHID-XACT-DREV-hid/", BOB)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    page(&test_db, "/transactions/detail/PHID-XACT-DREV-hid/", ALICE).await;

    let history = page(&test_db, "/transactions/history/PHID-XACT-DREV-com/", BOB).await;
    let first = history.find("<strong>draft</strong>").expect("version 1");
    let second = history.find("Second take").expect("version 2");
    assert!(first < second, "{history}");
    assert!(history.contains("Version 2"), "{history}");
    assert_eq!(
        get(&test_db, "/transactions/history/PHID-XACT-DREV-sum/", BOB)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // The current version of the comment, raw and quoted.
    let raw = page(&test_db, "/transactions/raw/PHID-XACT-DREV-com/", BOB).await;
    assert!(raw.contains(">Second take</textarea>"), "{raw}");
    assert!(!raw.contains("draft"), "{raw}");
    let quote = page(
        &test_db,
        "/transactions/quote/PHID-XACT-DREV-com/?ref=D1",
        BOB,
    )
    .await;
    assert_eq!(
        quote,
        "for (;;);{\"error\":null,\"payload\":{\"quoteText\":\"\\u003e\\u003e! In D1, @alice \
         wrote:\\n\\u003e Second take\"}}"
    );
    // The forms editing and removing it, whose posts PHP answers.
    let edit = page(&test_db, "/transactions/edit/PHID-XACT-DREV-com/", BOB).await;
    assert!(
        edit.contains(
            "<textarea name=\"text\" rows=\"12\" aria-label=\"Comment\">Second take</textarea>"
        ),
        "{edit}"
    );
    assert!(
        edit.contains("action=\"/transactions/edit/PHID-XACT-DREV-com/\""),
        "{edit}"
    );
    assert!(edit.contains("<a href=\"/D1\">Cancel</a>"), "{edit}");
    assert!(!edit.contains("signed with MFA"), "{edit}");
    let remove = page(&test_db, "/transactions/remove/PHID-XACT-DREV-com/", BOB).await;
    assert!(remove.contains("Really remove this comment?"), "{remove}");
    assert!(remove.contains("Remove Comment</button>"), "{remove}");
    for path in [
        "/transactions/edit/PHID-XACT-DREV-sum/",
        "/transactions/remove/PHID-XACT-DREV-sum/",
        "/transactions/raw/PHID-XACT-DREV-sum/",
        "/transactions/quote/PHID-XACT-DREV-sum/",
        "/transactions/raw/PHID-XACT-DREV-hid/",
    ] {
        assert_eq!(
            get(&test_db, path, BOB).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        fallback(
            &test_db,
            "/transactions/raw/PHID-XACT-DREV-com/?__ajax__=1",
            BOB
        )
        .await,
        "a transaction dialog"
    );
    assert_eq!(
        fallback(&test_db, "/transactions/raw/PHID-XACT-TASK-1/", BOB).await,
        "a transaction of an object not ported"
    );

    let subscribers = page(&test_db, "/subscriptions/list/PHID-DREV-1/", BOB).await;
    let bob = subscribers
        .find("<a href=\"/p/bob/\">bob</a>")
        .expect("bob");
    let alice = subscribers
        .find("<a href=\"/p/alice/\">alice</a>")
        .expect("alice");
    // The newest subscriber first.
    assert!(alice < bob, "{subscribers}");
    assert_eq!(
        fallback(&test_db, "/subscriptions/list/PHID-TASK-1/", BOB).await,
        "subscribers of an object not ported"
    );

    let flags = page(&test_db, "/flag/", ALICE).await;
    assert!(flags.contains("<a href=\"/D1\">D1: Open</a>"), "{flags}");
    assert!(flags.contains("Look again"), "{flags}");
    assert!(flags.contains("flag-green"), "{flags}");
    assert!(!flags.contains("Bob"), "{flags}");
    assert_eq!(
        fallback(&test_db, "/flag/query/abc/", ALICE).await,
        "a saved flag query"
    );
    test_db.drop_databases().await;
}
