//! The Ajax operations of the inline comment editor
//! (`/differential/comment/inline/edit/<id>/`) against a test database: the
//! rows they write, and what they leave to PHP.

// The checks share one database, so they are one long function.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use axum::body::{self, Body};
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
const ALICE: &str = "phsid=alice-token; phab_ui=rust";
const BOB: &str = "phsid=bob-token; phab_ui=rust";
const LEGACY_SOURCE: &str = r#"{"source":"legacy","params":[]}"#;

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

/// Alice's revision D1 with two changesets: 10 has two hunks, 11 one hunk
/// at the top of the file (whose context PHP caches). Bob has a draft on
/// each and a published comment on 10.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "differential",
        "repository",
        "project",
        "user",
        "policy",
        "spaces",
        "auth",
        "draft",
        "cache",
        "harbormaster",
        "search",
        "file",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, \
               isApproved, accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, '{secret}', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, '{secret}', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial, \
                highSecurityUntil) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1699000000, {expires}, 'PHID-SSSN-1', 0, NULL), \
             ('PHID-USER-bob', 'web', '{bob}', 1699000000, {expires}, 'PHID-SSSN-2', 0, NULL); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-1'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, repositoryPHID, sourceControlBaseRevision, lintStatus, unitStatus, \
               lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', NULL, 'abc', 0, 0, 3, 1, 1, 'users'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, phid, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, newProperties, \
               addLines, delLines, dateCreated, dateModified) VALUES \
             (10, 'PHID-DCHG-10', 1, 'a.c', 'a.c', '[]', 2, 1, '{{}}', '[]', '[]', 1, 1, 1, 1), \
             (11, 'PHID-DCHG-11', 1, 'b.c', 'b.c', '[]', 2, 1, '{{}}', '[]', '[]', 1, 1, 1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_hunk (changesetID, oldOffset, \
               oldLen, newOffset, newLen, dataType, dataEncoding, dataFormat, data, \
               dateCreated, dateModified) VALUES \
             (10, 5, 1, 5, 1, 'text', 'utf8', 'byte', ' x\\n', 1, 1), \
             (10, 20, 1, 20, 1, 'text', 'utf8', 'byte', ' y\\n', 1, 1), \
             (11, 1, 1, 1, 1, 'text', 'utf8', 'byte', ' z\\n', 1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_transaction_comment (id, phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, revisionPHID, changesetID, \
               isNewFile, lineNumber, lineLength, fixedState, hasReplies, replyToCommentPHID, \
               attributes) VALUES \
             (30, 'PHID-XCMT-30', NULL, 'PHID-USER-bob', 'public', 'PHID-USER-bob', 1, 'Nit', \
               '{{\"source\":\"web\"}}', 0, 1, 1, 'PHID-DREV-1', 10, 1, 5, 0, NULL, 0, NULL, \
               '{{\"editing\":true}}'), \
             (31, 'PHID-XCMT-31', 'PHID-XACT-DREV-31', 'PHID-USER-bob', 'public', \
               'PHID-USER-bob', 1, 'Published', '{{}}', 0, 1, 1, 'PHID-DREV-1', 10, 1, 5, 0, \
               NULL, 0, NULL, '[]'), \
             (32, 'PHID-XCMT-32', NULL, 'PHID-USER-bob', 'public', 'PHID-USER-bob', 1, 'Top', \
               '{{}}', 0, 1, 1, 'PHID-DREV-1', 11, 1, 1, 0, NULL, 0, NULL, '[]'); \
             INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 64, 'PHID-USER-bob', 1, 0); \
             INSERT INTO `{{ns}}_draft`.draft_versioneddraft (objectPHID, authorPHID, version, \
               properties, dateCreated, dateModified) VALUES \
             ('PHID-XCMT-30', 'PHID-USER-bob', 30, '{{\"inline.state\":{{\"text\":\"\"}}}}', 1, 1)",
            secret = ACCOUNT_SECRET,
            alice = session_key("alice-token"),
            bob = session_key("bob-token"),
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

async fn post(test_db: &TestDatabase, cookie: &str, pairs: &[(&str, &str)]) -> Response {
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
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(
            Request::post("/differential/comment/inline/edit/1/")
                .header(header::COOKIE, cookie)
                .header(header::HOST, "phabricator.test")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Posts `pairs` as `user`, with the `__ajax__` flag and a CSRF token.
async fn op(test_db: &TestDatabase, user: &str, pairs: &[(&str, &str)]) -> Response {
    let (cookie, phid, token) = if user == "alice" {
        (ALICE, "PHID-USER-alice", "alice-token")
    } else {
        (BOB, "PHID-USER-bob", "bob-token")
    };
    let csrf = csrf_token(test_db, phid, token).await;
    let mut all = vec![("__ajax__", "true"), ("__csrf__", csrf.as_str())];
    all.extend_from_slice(pairs);
    post(test_db, cookie, &all).await
}

async fn payload(response: Response) -> String {
    let status = response.status();
    let reason = response
        .headers()
        .get(FALLBACK_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(status, StatusCode::OK, "{reason:?} {text}");
    text
}

fn fallback_reason(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    response.headers()[FALLBACK_HEADER].to_str().unwrap()
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

async fn comment(test_db: &TestDatabase, id: i64) -> String {
    lines(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' | ', content, isDeleted, IFNULL(fixedState, '-'), editPolicy, \
               contentSource, attributes, dateModified) AS line \
             FROM `{{ns}}_differential`.differential_transaction_comment WHERE id = {id}"
        ),
    )
    .await
    .remove(0)
}

async fn has_draft_edge(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT(dst, ' ', dateCreated) AS line FROM `{ns}_differential`.edge \
         WHERE type = 64",
    )
    .await
}

const EMPTY: &str = r#"for (;;);{"error":null,"payload":{"inline":[],"view":null}}"#;

#[tokio::test]
async fn inline_comment_editor() {
    let Some(test_db) = setup().await else {
        return;
    };

    // Hiding and showing.
    let hidden = op(&test_db, "bob", &[("op", "hide"), ("ids", "30, 31,,")]).await;
    assert_eq!(
        payload(hidden).await,
        r#"for (;;);{"error":null,"payload":[]}"#
    );
    payload(op(&test_db, "bob", &[("op", "show"), ("ids", "31")]).await).await;
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT(userPHID, ' ', commentID) AS line \
             FROM `{ns}_differential`.differential_hiddencomment"
        )
        .await,
        vec!["PHID-USER-bob 30"]
    );
    assert_eq!(
        fallback_reason(&op(&test_db, "bob", &[("op", "hide"), ("ids", "3x")]).await),
        "IDs which are not integers"
    );

    // Done: Bob's own draft, then Alice (the author) on his published one.
    let done = op(&test_db, "bob", &[("op", "done"), ("id", "30")]).await;
    assert_eq!(
        payload(done).await,
        r#"for (;;);{"error":null,"payload":{"isChecked":true,"draftState":true}}"#
    );
    assert_eq!(
        comment(&test_db, 30).await,
        format!("Nit | 0 | draft | PHID-USER-bob | {LEGACY_SOURCE} | {{\"editing\":true}} | {NOW}")
    );
    assert_eq!(
        fallback_reason(&op(&test_db, "bob", &[("op", "done"), ("id", "31")]).await),
        "inline comments the viewer may not mark done"
    );
    payload(op(&test_db, "alice", &[("op", "done"), ("id", "31")]).await).await;
    assert_eq!(
        comment(&test_db, 31).await,
        format!("Published | 0 | draft | PHID-USER-bob | {LEGACY_SOURCE} | [] | {NOW}")
    );

    // A versioned draft of the editor's text.
    assert_eq!(
        payload(
            op(
                &test_db,
                "bob",
                &[("op", "draft"), ("id", "30"), ("text", "Nit!")]
            )
            .await
        )
        .await,
        EMPTY
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', objectPHID, version, properties, dateModified) AS line \
             FROM `{ns}_draft`.draft_versioneddraft"
        )
        .await,
        vec![format!(
            "PHID-XCMT-30 30 {{\"inline.state\":{{\"text\":\"Nit!\",\"hasSuggestion\":false,\
             \"suggestionText\":\"\"}}}} {NOW}"
        )]
    );
    assert_eq!(has_draft_edge(&test_db).await, vec!["PHID-USER-bob 1"]);

    // Deleting the drafts leaves Bob without any: the edge goes.
    payload(op(&test_db, "bob", &[("op", "delete"), ("id", "30")]).await).await;
    assert_eq!(
        comment(&test_db, 30).await,
        format!(
            "Nit | 1 | draft | PHID-USER-bob | {LEGACY_SOURCE} | {{\"editing\":false}} | {NOW}"
        )
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT objectPHID AS line FROM `{ns}_draft`.draft_versioneddraft"
        )
        .await,
        Vec::<String>::new()
    );
    // Draft 32 still has text: the edge stays as it was.
    assert_eq!(has_draft_edge(&test_db).await, vec!["PHID-USER-bob 1"]);
    // Without it, the edge goes, and comes back with the restored draft.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_transaction_comment SET content = '' \
             WHERE id = 32",
        )
        .await;
    payload(op(&test_db, "bob", &[("op", "delete"), ("id", "30")]).await).await;
    assert_eq!(has_draft_edge(&test_db).await, Vec::<String>::new());
    payload(op(&test_db, "bob", &[("op", "undelete"), ("id", "30")]).await).await;
    assert_eq!(
        has_draft_edge(&test_db).await,
        vec![format!("PHID-USER-bob {NOW}")]
    );

    // Closing the editor with its text.
    payload(
        op(
            &test_db,
            "bob",
            &[
                ("op", "cancel"),
                ("id", "30"),
                ("hasContentState", "1"),
                ("text", "Done <now>"),
                ("hasSuggestion", "true"),
                ("suggestionText", "x"),
            ],
        )
        .await,
    )
    .await;
    assert_eq!(
        comment(&test_db, 30).await,
        format!(
            "Done <now> | 0 | draft | PHID-USER-bob | {LEGACY_SOURCE} | {{\"editing\":false,\
             \"inline.state\":{{\"text\":\"Done <now>\",\"hasSuggestion\":true,\
             \"suggestionText\":\"x\"}}}} | {NOW}"
        )
    );

    // Left to PHP.
    for (pairs, reason) in [
        (vec![("op", "new")], "inline comment operations not ported"),
        (
            vec![("op", "save"), ("id", "30")],
            "inline comment operations not ported",
        ),
        (
            vec![("op", "refdelete"), ("id", "30")],
            "the confirmation of a deletion",
        ),
        (
            vec![("op", "cancel"), ("id", "30")],
            "cancels without a content state",
        ),
        (
            vec![("op", "delete"), ("id", "31")],
            "inline comments the viewer may not edit",
        ),
        (
            vec![("op", "delete"), ("id", "32")],
            "inline comments whose context PHP caches",
        ),
        (
            vec![("op", "edit"), ("replyToCommentPHID", "PHID-XCMT-31")],
            "replies to inline comments",
        ),
        (
            vec![("op", "delete"), ("id", "99")],
            "inline comments which don't load",
        ),
    ] {
        assert_eq!(
            fallback_reason(&op(&test_db, "bob", &pairs).await),
            reason,
            "{pairs:?}"
        );
    }
    assert_eq!(
        fallback_reason(&op(&test_db, "alice", &[("op", "delete"), ("id", "30")]).await),
        "inline comments the viewer may not edit"
    );
    assert_eq!(
        fallback_reason(&post(&test_db, BOB, &[("op", "hide"), ("ids", "1")]).await),
        "invalid CSRF tokens"
    );

    // Once PHP cached the context of 32, Rust deletes it.
    let key = "phabricator:inline(PHID-XCMT-32).context(v1)";
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_cache`.cache_general (cacheKeyHash, cacheKey, cacheFormat, \
               cacheData, cacheCreated, cacheExpires) VALUES ('{}', '{key}', 'raw', 'N;', 1, NULL)",
            hash::digest_for_index(key.as_bytes()),
        ))
        .await;
    payload(
        op(
            &test_db,
            "bob",
            &[("op", "refdelete"), ("id", "32"), ("__form__", "1")],
        )
        .await,
    )
    .await;
    assert!(comment(&test_db, 32).await.starts_with(" | 1 | "));
}
