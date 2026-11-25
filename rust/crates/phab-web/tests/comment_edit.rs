//! The posts editing and removing revision comments
//! (`/transactions/edit/<phid>/`, `/transactions/remove/<phid>/`) against a
//! test database: the rows they write, and what they leave to PHP.

// The checks share one database, so they are one long function.
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
use serde_json::{Map, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
/// A user's `accountSecret`, which fills its `binary(64)` column.
const ACCOUNT_SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;

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

/// Alice's revision D1, which Bob reviews, with a comment of each and one
/// of Carol, who isn't subscribed.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "differential",
        "repository",
        "project",
        "user",
        "policy",
        "spaces",
        "auth",
        "harbormaster",
        "search",
        "file",
    ])
    .await?;
    let mut sql = format!(
        "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
           conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, \
           isApproved, accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
         ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, '{ACCOUNT_SECRET}', 0, 0), \
         ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, '{ACCOUNT_SECRET}', 0, 0), \
         ('PHID-USER-carol', 'carol', 'Carol', 1, 1, 'c', 0, 0, 0, 1, 1, '{ACCOUNT_SECRET}', 0, 0); \
         INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
         VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
         INSERT INTO `{{ns}}_user`.phabricator_session \
           (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial, \
            highSecurityUntil) VALUES \
         ('PHID-USER-alice', 'web', '{alice}', 1699000000, {expires}, 'PHID-SSSN-1', 0, NULL), \
         ('PHID-USER-bob', 'web', '{bob}', 1699000000, {expires}, 'PHID-SSSN-2', 0, NULL), \
         ('PHID-USER-carol', 'web', '{carol}', 1699000000, {expires}, 'PHID-SSSN-3', 0, NULL); \
         INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
           summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
           viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
         (1, 'Open', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
           'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-1'); \
         INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, \
           reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
         ('PHID-DREV-1', 'PHID-USER-bob', 'added', 1, 1, '{{}}'); \
         INSERT INTO `{{ns}}_differential`.differential_transaction (phid, authorPHID, \
           objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, \
           oldValue, newValue, contentSource, metadata, dateCreated, dateModified) VALUES ",
        alice = session_key("alice-token"),
        bob = session_key("bob-token"),
        carol = session_key("carol-token"),
        expires = NOW + 29 * 86_400,
    );
    let comments = [
        ("bob", "{}", "Hello"),
        ("alice", "{\"core.groupID\":\"g\"}", "Mine"),
        ("carol", "{}", "Drive-by"),
        ("mfa", "{\"core.mfa\":true}", "Signed"),
        ("inl", "{}", "Inline"),
    ];
    let xactions: Vec<String> = comments
        .iter()
        .map(|(key, metadata, _)| {
            let author = match *key {
                "mfa" | "inl" => "bob",
                author => author,
            };
            let kind = if *key == "inl" {
                "differential:inline"
            } else {
                "core:comment"
            };
            format!(
                "('PHID-XACT-DREV-{key}000000', 'PHID-USER-{author}', 'PHID-DREV-1', 'public', \
                 'PHID-USER-{author}', 'PHID-XCMT-{key}000000', 1, '{kind}', 'null', 'null', \
                 '{WEB_SOURCE}', '{metadata}', 5, 5)"
            )
        })
        .collect();
    sql.push_str(&xactions.join(", "));
    sql.push_str(
        "; INSERT INTO `{ns}_differential`.differential_transaction_comment (phid, \
           transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
           contentSource, isDeleted, dateCreated, dateModified, isNewFile, lineNumber, \
           lineLength, hasReplies, attributes) VALUES ",
    );
    let rows: Vec<String> = comments
        .iter()
        .map(|(key, _, text)| {
            let author = match *key {
                "mfa" | "inl" => "bob",
                author => author,
            };
            format!(
                "('PHID-XCMT-{key}000000', 'PHID-XACT-DREV-{key}000000', 'PHID-USER-{author}', \
                 'public', 'PHID-USER-{author}', 1, '{text}', '{WEB_SOURCE}', 0, 5, 5, 0, 0, 0, \
                 0, '[]')"
            )
        })
        .collect();
    sql.push_str(&rows.join(", "));
    test_db.execute(&sql).await;
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

/// Posts the form of `action` on comment `key` as `user`, with `extra`.
async fn post(
    test_db: &TestDatabase,
    user: &str,
    action: &str,
    key: &str,
    extra: &[(&str, &str)],
) -> Response {
    let mut config = Config::default();
    let mut values = Map::new();
    values.insert(
        "phabricator.base-uri".to_owned(),
        json!("http://phabricator.test/"),
    );
    config.push_layer("test", values);
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let token = csrf_token(test_db, user).await;
    let mut pairs = vec![("__csrf__", token.as_str())];
    pairs.extend_from_slice(extra);
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(
            Request::post(format!(
                "/transactions/{action}/PHID-XACT-DREV-{key}000000/"
            ))
            .header(header::COOKIE, format!("phsid={user}-token; phab_ui=rust"))
            .header(header::HOST, "phabricator.test")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
        )
        .await
        .unwrap()
}

fn fallback_reason(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    response.headers()[FALLBACK_HEADER].to_str().unwrap()
}

fn assert_saved(response: &Response) {
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
}

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

/// The versions of comment `key`, and its transaction.
async fn versions(test_db: &TestDatabase, key: &str) -> Vec<String> {
    let mut found = lines(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' | ', commentVersion, content, isDeleted, authorPHID, \
               viewPolicy, editPolicy, contentSource, attributes, dateModified) AS line \
             FROM `{{ns}}_differential`.differential_transaction_comment \
             WHERE transactionPHID = 'PHID-XACT-DREV-{key}000000' ORDER BY commentVersion"
        ),
    )
    .await;
    found.extend(
        lines(
            test_db,
            &format!(
                "SELECT CONCAT_WS(' | ', x.commentVersion, x.editPolicy, x.metadata, \
                   x.dateModified, c.commentVersion) AS line \
                 FROM `{{ns}}_differential`.differential_transaction x \
                 JOIN `{{ns}}_differential`.differential_transaction_comment c \
                   ON c.phid = x.commentPHID \
                 WHERE x.phid = 'PHID-XACT-DREV-{key}000000'"
            ),
        )
        .await,
    );
    found
}

#[tokio::test]
async fn comment_edits_and_removals() {
    let Some(test_db) = setup().await else {
        return;
    };

    // Bob edits his comment, then empties it.
    let form = [("__form__", "1"), ("text", "Hello again")];
    assert_saved(&post(&test_db, "bob", "edit", "bob", &form).await);
    assert_eq!(
        versions(&test_db, "bob").await,
        vec![
            format!(
                "1 | Hello | 0 | PHID-USER-bob | public | PHID-USER-bob | {WEB_SOURCE} | [] | 5"
            ),
            format!(
                "2 | Hello again | 0 | PHID-USER-bob | public | PHID-USER-bob | {WEB_SOURCE} | [] \
                 | {NOW}"
            ),
            format!("2 | PHID-USER-bob | [] | {NOW} | 2"),
        ]
    );
    assert_saved(
        &post(
            &test_db,
            "bob",
            "edit",
            "bob",
            &[("__form__", "1"), ("text", "")],
        )
        .await,
    );
    assert!(
        versions(&test_db, "bob").await[2].starts_with("3 |  | 1 | PHID-USER-bob"),
        "{:?}",
        versions(&test_db, "bob").await
    );
    // An emptied comment goes to PHP.
    assert_eq!(
        fallback_reason(&post(&test_db, "bob", "edit", "bob", &form).await),
        "a removed comment"
    );

    // Alice removes hers.
    assert_saved(&post(&test_db, "alice", "remove", "alice", &[("__form__", "1")]).await);
    assert_eq!(
        versions(&test_db, "alice").await[1..],
        [
            format!(
                "2 |  | 2 | PHID-USER-alice | public | PHID-USER-alice | {WEB_SOURCE} | [] | {NOW}"
            ),
            format!("2 | PHID-USER-alice | {{\"core.groupID\":\"g\"}} | {NOW} | 2"),
        ]
    );
    assert_eq!(
        fallback_reason(&post(&test_db, "alice", "remove", "alice", &[("__form__", "1")]).await),
        "a removed comment"
    );

    // Left to PHP.
    for (user, action, key, extra, reason) in [
        (
            "bob",
            "edit",
            "carol",
            vec![("__form__", "1"), ("text", "x")],
            "comments of other users",
        ),
        (
            "carol",
            "edit",
            "carol",
            vec![("__form__", "1"), ("text", "x")],
            "comment edits subscribing the viewer",
        ),
        (
            "bob",
            "edit",
            "mfa",
            vec![("__form__", "1"), ("text", "x")],
            "comments signed with MFA",
        ),
        (
            "bob",
            "edit",
            "inl",
            vec![("__form__", "1"), ("text", "x")],
            "comments of other transactions",
        ),
        (
            "alice",
            "edit",
            "carol",
            vec![("__form__", "1"), ("text", "x")],
            "comments of other users",
        ),
        (
            "carol",
            "edit",
            "carol",
            vec![("text", "x")],
            "requests which aren't form posts",
        ),
        (
            "carol",
            "remove",
            "carol",
            vec![("__form__", "1"), ("__ajax__", "1")],
            "a workflow request",
        ),
    ] {
        assert_eq!(
            fallback_reason(&post(&test_db, user, action, key, &extra).await),
            reason,
            "{user} {action} {key}"
        );
    }
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_transaction (phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, \
               oldValue, newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('PHID-XACT-DREV-men000000', 'PHID-USER-bob', 'PHID-DREV-1', 'public', \
               'PHID-USER-bob', 'PHID-XCMT-men000000', 1, 'core:comment', 'null', 'null', \
               '{}', '{}', 5, 5); \
             INSERT INTO `{ns}_differential`.differential_transaction_comment (phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, isNewFile, lineNumber, \
               lineLength, hasReplies, attributes) VALUES \
             ('PHID-XCMT-men000000', 'PHID-XACT-DREV-men000000', 'PHID-USER-bob', 'public', \
               'PHID-USER-bob', 1, 'x', '{}', 0, 5, 5, 0, 0, 0, 0, '[]')",
        )
        .await;
    assert_eq!(
        fallback_reason(
            &post(
                &test_db,
                "bob",
                "edit",
                "men",
                &[("__form__", "1"), ("text", "@alice?")]
            )
            .await
        ),
        "comments with remarkup effects"
    );
    // Carol, once subscribed, edits hers.
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 21, 'PHID-USER-carol', 1, 0)",
        )
        .await;
    assert_saved(
        &post(
            &test_db,
            "carol",
            "edit",
            "carol",
            &[("__form__", "1"), ("text", "Better")],
        )
        .await,
    );
}
