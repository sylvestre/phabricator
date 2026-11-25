//! The small pages of a revision: why a commit closed it, its inline
//! comments and its affected path index.

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
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy, repositorySlug) VALUES \
             (1, 'PHID-REPO-git', 'project', 'PROJECT', 'git', '{{}}', 1, 1, 'users', 'users', \
               'users', 'project'), \
             (2, 'PHID-REPO-hid', 'secret', 'SECRET', 'git', '{{}}', 1, 1, 'PHID-USER-alice', \
               'users', 'users', 'secret'); \
             INSERT INTO `{{ns}}_repository`.repository_path (id, path, pathHash) VALUES \
             (5, '/src/b.c', 'h5'), (6, '/src/a.c', 'h6'), (7, '/hidden', 'h7'); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'published', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', '{{}}', 'PHID-DIFF-8'), \
             (2, 'Hidden', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'PHID-USER-alice', 'users', '{{}}', 'PHID-DIFF-x'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, repositoryPHID, sourceControlBaseRevision, lintStatus, unitStatus, \
               lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (7, 'PHID-DIFF-7', 1, 'PHID-USER-alice', NULL, 'abc', 0, 0, 3, 1, 1, 'users'), \
             (8, 'PHID-DIFF-8', 1, 'PHID-USER-alice', NULL, 'abc', 0, 0, 3, 1, 1, 'users'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, phid, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, newProperties, \
               addLines, delLines, dateCreated, dateModified) VALUES \
             (10, 'PHID-DCHG-10', 7, 'a.c', 'a.c', '[]', 2, 1, '{{}}', '[]', '[]', 1, 1, 1, 1), \
             (11, 'PHID-DCHG-11', 8, 'a.c', 'a.c', '[]', 2, 1, '{{}}', '[]', '[]', 1, 1, 1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_transaction (id, phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, \
               transactionType, oldValue, newValue, contentSource, metadata, dateCreated, \
               dateModified) VALUES \
             (1, 'PHID-XACT-DREV-close', 'PHID-USER-alice', 'PHID-DREV-1', 'public', 'users', \
               NULL, 0, 'differential.revision.close', 'false', 'true', '{{}}', \
               '{{\"revisionMatchData\":{{\"usedURI\":true,\"foundURI\":\"http://phab.test/D1\",\
               \"validDomain\":\"phab.test\",\"matchHashType\":null,\"matchHashValue\":null}}}}', \
               10, 10), \
             (2, 'PHID-XACT-DREV-old', 'PHID-USER-alice', 'PHID-DREV-1', 'public', 'users', \
               NULL, 0, 'differential.revision.close', 'false', 'true', '{{}}', '{{}}', 10, 10), \
             (3, 'PHID-XACT-DREV-odd', 'PHID-USER-alice', 'PHID-DREV-1', 'public', 'users', \
               NULL, 0, 'differential.revision.close', 'false', 'true', '{{}}', \
               '{{\"revisionMatchData\":{{\"matchHashType\":true}}}}', 10, 10), \
             (4, 'PHID-XACT-DREV-hid', 'PHID-USER-alice', 'PHID-DREV-2', 'public', 'users', \
               NULL, 0, 'differential.revision.close', 'false', 'true', '{{}}', '{{}}', 10, 10), \
             (5, 'PHID-XACT-DREV-inl', 'PHID-USER-bob', 'PHID-DREV-1', 'public', 'users', \
               NULL, 0, 'differential:inline', 'null', 'null', '{{}}', '{{}}', 10, 10); \
             INSERT INTO `{{ns}}_differential`.differential_transaction_comment (id, phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, revisionPHID, changesetID, \
               isNewFile, lineNumber, lineLength, fixedState, hasReplies, replyToCommentPHID, \
               attributes) VALUES \
             (20, 'PHID-XCMT-20', 'PHID-XACT-DREV-i20', 'PHID-USER-bob', 'public', 'users', 1, \
               'Old diff <note>', '{{}}', 0, 1, {two_hours}, 'PHID-DREV-1', 10, 1, 3, 0, NULL, \
               1, NULL, '{{}}'), \
             (21, 'PHID-XCMT-21', 'PHID-XACT-DREV-i21', 'PHID-USER-alice', 'public', 'users', \
               1, 'Fixed', '{{}}', 0, 1, {two_hours}, 'PHID-DREV-1', 10, 1, 3, 0, NULL, 0, \
               'PHID-XCMT-20', '{{}}'), \
             (22, 'PHID-XCMT-22', 'PHID-XACT-DREV-i22', 'PHID-USER-bob', 'public', 'users', 1, \
               'Current one', '{{}}', 0, 1, {two_hours}, 'PHID-DREV-1', 11, 1, 3, 0, 'done', 0, \
               NULL, '{{}}'), \
             (23, 'PHID-XCMT-23', NULL, 'PHID-USER-bob', 'public', 'users', 1, 'A draft', \
               '{{}}', 0, 1, 1, 'PHID-DREV-1', 11, 1, 3, 0, NULL, 0, NULL, '{{}}'), \
             (24, 'PHID-XCMT-24', 'PHID-XACT-DREV-i24', 'PHID-USER-bob', 'PHID-USER-bob', \
               'users', 1, 'Only for Bob', '{{}}', 0, 1, 1, 'PHID-DREV-1', 11, 1, 3, 0, NULL, 0, \
               NULL, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.differential_affectedpath (repositoryID, pathID, \
               revisionID) VALUES (1, 5, 1), (1, 6, 1), (2, 7, 1), (NULL, 99, 1)",
            alice = session_key("alice-token"),
            bob = session_key("bob-token"),
            expires = NOW + 29 * 86_400,
            two_hours = NOW - 2 * 3600,
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
async fn small_revision_pages() {
    let Some(test_db) = setup().await else {
        return;
    };

    // Why a commit closed the revision.
    let close = page(
        &test_db,
        "/differential/revision/closedetails/PHID-XACT-DREV-close/",
        BOB,
    )
    .await;
    assert!(
        close.contains(
            "with value \"http://phab.test/D1\" in the commit message, and the domain on the URI \
             matches this install, so we linked this commit to <a href=\"/D1\" \
             class=\"remarkup-object-closed\">D1</a>."
        ),
        "{close}"
    );
    let old = page(
        &test_db,
        "/differential/revision/closedetails/PHID-XACT-DREV-old/",
        BOB,
    )
    .await;
    assert!(
        old.contains("This commit was made before this feature was built"),
        "{old}"
    );
    assert_eq!(
        fallback(
            &test_db,
            "/differential/revision/closedetails/PHID-XACT-DREV-odd/",
            BOB
        )
        .await,
        "unusual revision match data"
    );
    assert_eq!(
        fallback(
            &test_db,
            "/differential/revision/closedetails/PHID-XACT-DREV-close/?__ajax__=1",
            BOB
        )
        .await,
        "a transaction dialog"
    );
    assert_eq!(
        fallback(
            &test_db,
            "/differential/revision/closedetails/PHID-XACT-CMIT-1/",
            BOB
        )
        .await,
        "a transaction of an object not ported"
    );
    // Only Alice sees D2.
    assert_eq!(
        get(
            &test_db,
            "/differential/revision/closedetails/PHID-XACT-DREV-hid/",
            BOB
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    page(
        &test_db,
        "/differential/revision/closedetails/PHID-XACT-DREV-hid/",
        ALICE,
    )
    .await;
    // Logged out: PHP asks to log in.
    assert_eq!(
        get(
            &test_db,
            "/differential/revision/closedetails/PHID-XACT-DREV-close/",
            ""
        )
        .await
        .status(),
        StatusCode::MISDIRECTED_REQUEST
    );

    // The inline comments: threaded, the reply by the author, drafts and
    // comments the viewer can't see left out.
    let inlines = page(&test_db, "/differential/revision/inlines/1/", ALICE).await;
    let first = inlines.find("Old diff &#60;note&#62;").expect(&inlines);
    let reply = inlines.find(">Fixed</a>").expect(&inlines);
    let current = inlines.find(">Current one</a>").expect(&inlines);
    assert!(first < reply && reply < current, "{inlines}");
    assert!(inlines.contains("Diff 7"), "{inlines}");
    assert!(inlines.contains("<strong>Current</strong>"), "{inlines}");
    assert!(inlines.contains("fa-commenting-o blue"), "{inlines}");
    assert!(inlines.contains("fa-check green"), "{inlines}");
    assert!(inlines.contains("href=\"/D1#inline-21\""), "{inlines}");
    assert!(inlines.contains("2 h"), "{inlines}");
    assert!(!inlines.contains("A draft"), "{inlines}");
    assert!(!inlines.contains("Only for Bob"), "{inlines}");
    let for_bob = page(&test_db, "/differential/revision/inlines/1/", BOB).await;
    assert!(for_bob.contains("Only for Bob"), "{for_bob}");
    assert_eq!(
        fallback(&test_db, "/differential/revision/inlines/2/", BOB).await,
        "policy exceptions"
    );
    assert_eq!(
        get(&test_db, "/differential/revision/inlines/9/", BOB)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    page(&test_db, "/differential/revision/inlines/2/", ALICE).await;

    // The affected paths, by path, with the repositories the viewer sees.
    let paths = page(&test_db, "/differential/revision/paths/1/", BOB).await;
    let a = paths.find("/src/a.c").expect(&paths);
    let b = paths.find("/src/b.c").expect(&paths);
    assert!(a < b, "{paths}");
    assert!(
        paths.contains("<a href=\"/source/project/\">rPROJECT project</a>"),
        "{paths}"
    );
    assert!(paths.contains("/hidden"), "{paths}");
    assert!(!paths.contains("secret"), "{paths}");
    assert!(paths.contains("<td>99</td>"), "{paths}");
    let for_alice = page(&test_db, "/differential/revision/paths/1/", ALICE).await;
    assert!(for_alice.contains("secret"), "{for_alice}");
    let empty = page(&test_db, "/differential/revision/paths/2/", ALICE).await;
    assert!(
        empty.contains("This revision has no indexed affected paths."),
        "{empty}"
    );
    assert_eq!(
        fallback(&test_db, "/differential/revision/paths/2/", BOB).await,
        "policy exceptions"
    );
}
