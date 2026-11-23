//! `/feed/`, its queries and stories, and `/notification/` against a test
//! database.

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::WebState;
use serde_json::json;
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const EPOCH: u64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
const STORY: &str = "PhabricatorApplicationTransactionFeedStory";

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

fn chronological_key(id: u64) -> u64 {
    (EPOCH << 32) + id
}

/// A story `id` by `author` about `object`, made of one transaction.
async fn add_story(
    test_db: &TestDatabase,
    id: u64,
    author: &str,
    object: &str,
    kind: &str,
    values: (&str, &str, &str),
) {
    let transaction = format!("PHID-XACT-DREV-{id}");
    let data = json!({
        "objectPHID": object,
        "transactionPHIDs": {&transaction: &transaction},
    })
    .to_string();
    let key = chronological_key(id);
    let (old, new, metadata) = values;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_transaction (phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, \
               oldValue, newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('{transaction}', '{author}', '{object}', 'public', '{author}', NULL, 0, \
               '{kind}', '{old}', '{new}', '{{}}', '{metadata}', 1, 1); \
             INSERT INTO `{{ns}}_feed`.feed_storydata (id, phid, chronologicalKey, storyType, \
               storyData, authorPHID, dateCreated, dateModified) VALUES \
             ({id}, 'PHID-STRY-{id}', {key}, '{STORY}', '{data}', '{author}', 1, 1); \
             INSERT INTO `{{ns}}_feed`.feed_storyreference (objectPHID, chronologicalKey) \
             VALUES ('{object}', {key}), ('{author}', {key})"
        ))
        .await;
}

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "user",
        "auth",
        "feed",
        "differential",
        "project",
        "policy",
        "spaces",
        "repository",
    ])
    .await?;
    let session = hash::hmac_sha256(b"alice-token", &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap();
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
             ('PHID-USER-alice', 'web', '{session}', 1, {expires}, 'PHID-SSSN-1', 0); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, properties, activeDiffPHID) VALUES \
             (1, 'Open <one>', 'PHID-DREV-one', 'accepted', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', '{{}}', ''), \
             (2, 'Secret', 'PHID-DREV-two', 'needs-review', '', '', 'PHID-USER-bob', 1, 1, '{{}}', \
               'k', 'no-one', 'no-one', '{{}}', '')",
            expires = NOW + 29 * 86_400,
        ))
        .await;
    // 1: bob accepts D1. 2: bob accepts D2, which alice can't see.
    // 3: alice retitles D1. 4: an edge type whose title is not ported.
    // 5: a story about a commit, which is not ported.
    add_story(
        &test_db,
        1,
        "PHID-USER-bob",
        "PHID-DREV-one",
        "differential.revision.accept",
        ("null", "true", "{}"),
    )
    .await;
    add_story(
        &test_db,
        2,
        "PHID-USER-bob",
        "PHID-DREV-two",
        "differential.revision.accept",
        ("null", "true", "{}"),
    )
    .await;
    add_story(
        &test_db,
        3,
        "PHID-USER-alice",
        "PHID-DREV-one",
        "differential.revision.title",
        ("\"Old\"", "\"Open <one>\"", "{}"),
    )
    .await;
    add_story(
        &test_db,
        4,
        "PHID-USER-alice",
        "PHID-DREV-one",
        "core:edge",
        ("[]", "[]", "{\"edge:type\":99}"),
    )
    .await;
    add_story(
        &test_db,
        5,
        "PHID-USER-alice",
        "PHID-CMIT-one",
        "differential.revision.accept",
        ("null", "true", "{}"),
    )
    .await;
    Some(test_db)
}

async fn get(test_db: &TestDatabase, path: &str) -> (StatusCode, String) {
    let config = Config::default();
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let response = phab_web::router::<State>()
        .with_state(state)
        .oneshot(
            Request::get(path)
                .header(header::COOKIE, "phsid=alice-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn feed_shows_visible_stories_with_titles() {
    let Some(test_db) = setup().await else {
        return;
    };
    let (status, page) = get(&test_db, "/feed/").await;
    assert_eq!(status, StatusCode::OK);
    let retitle = page
        .find("<a href=\"/p/alice/\">alice</a> retitled <a href=\"/D1\">D1: Open &lt;one&gt;</a> from \
                <span class=\"title-value\">Old</span> to <span class=\"title-value\">Open &lt;one&gt;</span>.")
        .unwrap_or_else(|| panic!("{page}"));
    let accept = page
        .find("<a href=\"/p/bob/\">bob</a> accepted <a href=\"/D1\">D1: Open &lt;one&gt;</a>.")
        .unwrap_or_else(|| panic!("{page}"));
    // Newest first; D2 is not shown at all.
    assert!(retitle < accept);
    assert!(!page.contains("Secret"), "{page}");
    assert!(page.contains("2023-11-14 22:13 UTC"), "{page}");
    // The edge and the commit stories are left to the old interface.
    assert!(page.contains("2 stories are not shown here yet"), "{page}");
    assert!(!page.contains(">Older<"), "{page}");

    // Older than story 3: only story 1, with a link back to newer ones.
    let (_, page) = get(&test_db, &format!("/feed/?after={}", chronological_key(3))).await;
    assert!(page.contains("bob</a> accepted"), "{page}");
    assert!(!page.contains("retitled"), "{page}");
    assert!(
        page.contains(&format!("/feed/?before={}", chronological_key(1))),
        "{page}"
    );

    // Newer than story 1: story 3 only.
    let (_, page) = get(&test_db, &format!("/feed/?before={}", chronological_key(1))).await;
    assert!(page.contains("retitled"), "{page}");
    assert!(!page.contains("accepted"), "{page}");

    // The stories referencing bob, on the feed and on his profile.
    let (_, page) = get(&test_db, "/feed/?userPHIDs=PHID-USER-bob").await;
    assert!(page.contains("bob</a> accepted"), "{page}");
    assert!(!page.contains("retitled"), "{page}");
    let (status, page) = get(&test_db, "/p/bob/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("bob</a> accepted"), "{page}");
    assert!(!page.contains("retitled"), "{page}");
    assert!(!page.contains("not shown here yet"), "{page}");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn builtin_queries_and_single_stories() {
    let Some(test_db) = setup().await else {
        return;
    };
    // Without projects, the "projects" query has no filter: every story.
    let (status, page) = get(&test_db, "/feed/query/projects/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<h1>Tags</h1>"), "{page}");
    assert!(page.contains("retitled"), "{page}");
    assert!(page.contains("accepted"), "{page}");

    // With one, only the stories about it.
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_project`.project (name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug) VALUES \
             ('Build', 'PHID-PROJ-build', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', \
               'blue', 'k', 0, 0, 0, 'a', 0, 'k001', '{{}}', 'default', 'build'); \
             INSERT INTO `{{ns}}_user`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-USER-alice', 14, 'PHID-PROJ-build', 1, 0); \
             INSERT INTO `{{ns}}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
               ('PHID-PROJ-build', 13, 'PHID-USER-alice', 1, 0); \
             INSERT INTO `{{ns}}_feed`.feed_storyreference (objectPHID, chronologicalKey) \
             VALUES ('PHID-PROJ-build', {key})",
            key = chronological_key(1),
        ))
        .await;
    let (_, page) = get(&test_db, "/feed/query/projects/").await;
    assert!(page.contains("bob</a> accepted"), "{page}");
    assert!(!page.contains("retitled"), "{page}");
    let (_, page) = get(&test_db, "/feed/query/all/").await;
    assert!(page.contains("<h1>All Stories</h1>"), "{page}");
    assert!(page.contains("retitled"), "{page}");

    let (status, _) = get(&test_db, "/feed/query/abcdef123456/").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);

    // One story, by chronological key.
    let (status, page) = get(&test_db, &format!("/feed/{}/", chronological_key(3))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("<title>Story · Phabricator</title>"),
        "{page}"
    );
    assert!(page.contains("alice</a> retitled"), "{page}");
    assert!(!page.contains("accepted"), "{page}");
    // Hidden, unknown, and not ported.
    for (path, expected) in [
        (
            format!("/feed/{}/", chronological_key(2)),
            StatusCode::NOT_FOUND,
        ),
        ("/feed/99/".to_owned(), StatusCode::NOT_FOUND),
        (
            format!("/feed/{}/", chronological_key(4)),
            StatusCode::MISDIRECTED_REQUEST,
        ),
        (
            format!("/feed/{}/?text=1", chronological_key(3)),
            StatusCode::MISDIRECTED_REQUEST,
        ),
    ] {
        let (status, _) = get(&test_db, &path).await;
        assert_eq!(status, expected, "{path}");
    }
    test_db.drop_databases().await;
}

#[tokio::test]
async fn notifications_mark_the_unread_ones() {
    let Some(test_db) = setup().await else {
        return;
    };
    // alice was notified of stories 1 (viewed), 3 and 2 (hidden from her);
    // bob of story 1.
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_feed`.feed_storynotification (userPHID, primaryObjectPHID, \
               chronologicalKey, hasViewed) VALUES \
             ('PHID-USER-alice', 'PHID-DREV-one', {one}, 1), \
             ('PHID-USER-alice', 'PHID-DREV-one', {three}, 0), \
             ('PHID-USER-alice', 'PHID-DREV-two', {two}, 0), \
             ('PHID-USER-bob', 'PHID-DREV-one', {one}, 0)",
            one = chronological_key(1),
            two = chronological_key(2),
            three = chronological_key(3),
        ))
        .await;
    let (status, page) = get(&test_db, "/notification/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<h1>Notifications</h1>"), "{page}");
    let retitle = page
        .find(
            "<li class=\"feed-story notification-unread\"><span class=\"tag\">Unread</span> \
             <span class=\"feed-title\"><a href=\"/p/alice/\">alice</a> retitled",
        )
        .unwrap_or_else(|| panic!("{page}"));
    let accept = page
        .find(
            "<li class=\"feed-story\"><span class=\"feed-title\"><a href=\"/p/bob/\">bob</a> \
             accepted",
        )
        .unwrap_or_else(|| panic!("{page}"));
    assert!(retitle < accept, "{page}");
    assert!(!page.contains("Secret"), "{page}");

    let (status, page) = get(&test_db, "/notification/query/unread/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<h1>Unread Notifications</h1>"), "{page}");
    assert!(page.contains("retitled"), "{page}");
    assert!(!page.contains("accepted"), "{page}");

    test_db
        .execute("UPDATE `{ns}_feed`.feed_storynotification SET hasViewed = 1")
        .await;
    let (_, page) = get(&test_db, "/notification/query/unread/").await;
    assert!(page.contains("You have no unread notifications."), "{page}");

    for path in ["/notification/?unread=1", "/notification/query/saved123/"] {
        let (status, _) = get(&test_db, path).await;
        assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{path}");
    }
    test_db.drop_databases().await;
}
