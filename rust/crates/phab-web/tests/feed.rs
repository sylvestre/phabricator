//! `/feed/`, its queries and stories, and `/notification/` against a test
//! database.

// Each test fills its database with stories, then checks the pages.
#![allow(clippy::too_many_lines)]

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
        "audit",
        "harbormaster",
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
    // 5: a story about a commit made of revision transactions, left to PHP.
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

/// A story `id` about a commit, made of one `audit_transaction`.
async fn add_commit_story(
    test_db: &TestDatabase,
    id: u64,
    (author, commit): (&str, &str),
    kind: &str,
    (new, metadata): (&str, &str),
) {
    let transaction = format!("PHID-XACT-CMIT-{id}");
    let data = json!({
        "objectPHID": commit,
        "transactionPHIDs": [&transaction],
    })
    .to_string();
    let key = chronological_key(id);
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_audit`.audit_transaction (phid, authorPHID, objectPHID, \
               viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, oldValue, \
               newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('{transaction}', '{author}', '{commit}', 'public', 'no-one', NULL, 0, '{kind}', \
               'null', '{new}', '{{}}', '{metadata}', 1, 1); \
             INSERT INTO `{{ns}}_feed`.feed_storydata (id, phid, chronologicalKey, storyType, \
               storyData, authorPHID, dateCreated, dateModified) VALUES \
             ({id}, 'PHID-STRY-{id}', {key}, '{STORY}', '{data}', '{author}', 1, 1); \
             INSERT INTO `{{ns}}_feed`.feed_storyreference (objectPHID, chronologicalKey) \
             VALUES ('{commit}', {key}), ('{author}', {key})"
        ))
        .await;
}

#[tokio::test]
async fn commit_and_buildable_stories() {
    let Some(test_db) = setup().await else {
        return;
    };
    // A repository alice can see, with a commit and a rewritten one, and a
    // repository she can't see.
    test_db
        .execute(
            "INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy, repositorySlug) VALUES \
             (1, 'PHID-REPO-vct', 'Tools', 'VCT', 'git', '{}', 1, 1, 'users', 'users', 'users', \
               NULL), \
             (2, 'PHID-REPO-secret', 'Secret', 'SECRET', 'git', '{}', 1, 1, 'no-one', 'no-one', \
               'no-one', NULL); \
             INSERT INTO `{ns}_repository`.repository_commit (repositoryID, phid, \
               commitIdentifier, epoch, auditStatus, summary, importStatus) VALUES \
             (1, 'PHID-CMIT-landed', 'abcdef0123456789abcdef0123456789abcdef01', 1, 'none', \
               'Add <the> README', 15), \
             (1, 'PHID-CMIT-gone', 'b50ab917138e4e2fb5bf04a99d8eacf540934cdb', 1, 'none', \
               'Rewritten', 3083), \
             (2, 'PHID-CMIT-hidden', '1111111111111111111111111111111111111111', 1, 'none', \
               'Hidden', 15); \
             INSERT INTO `{ns}_repository`.repository_commithint (repositoryPHID, \
               oldCommitIdentifier, newCommitIdentifier, hintType) VALUES \
             ('PHID-REPO-vct', 'b50ab917138e4e2fb5bf04a99d8eacf540934cdb', \
               'cd825461a99bc447c59d79f92156325664434c44', 'rewritten'); \
             INSERT INTO `{ns}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'); \
             INSERT INTO `{ns}_harbormaster`.harbormaster_buildable (id, phid, buildablePHID, \
               containerPHID, buildableStatus, dateCreated, dateModified, isManualBuildable) \
             VALUES (1, 'PHID-HMBB-1', 'PHID-DIFF-1', 'PHID-DREV-one', 'failed', 1, 1, 0)",
        )
        .await;
    // 6: alice commits, bob wrote it. 7: a commit by someone without an
    // account. 8: the rewritten commit. 9: a hidden commit. 10: a
    // Harbormaster failure on D1. 11: a passed build, hidden from feed.
    add_commit_story(
        &test_db,
        6,
        ("PHID-USER-alice", "PHID-CMIT-landed"),
        "audit:commit",
        (
            r#"{"summary":"Add <the> README","authorName":"bob","authorPHID":"PHID-USER-bob","committerName":"alice","committerPHID":"PHID-USER-alice"}"#,
            "{}",
        ),
    )
    .await;
    add_commit_story(
        &test_db,
        7,
        ("PHID-RIDT-someone", "PHID-CMIT-landed"),
        "audit:commit",
        (
            r#"{"summary":"","authorName":"Someone <someone@example.com>","authorPHID":null,"committerName":null,"committerPHID":null}"#,
            "{}",
        ),
    )
    .await;
    add_commit_story(
        &test_db,
        8,
        ("PHID-USER-alice", "PHID-CMIT-gone"),
        "audit:commit",
        (
            r#"{"summary":"Rewritten","authorName":"alice","authorPHID":"PHID-USER-alice","committerName":null,"committerPHID":null}"#,
            "{}",
        ),
    )
    .await;
    add_commit_story(
        &test_db,
        9,
        ("PHID-USER-bob", "PHID-CMIT-hidden"),
        "audit:commit",
        (
            r#"{"summary":"Hidden","authorName":"bob","authorPHID":"PHID-USER-bob","committerName":null,"committerPHID":null}"#,
            "{}",
        ),
    )
    .await;
    add_story(
        &test_db,
        10,
        "PHID-APPS-PhabricatorHarbormasterApplication",
        "PHID-DREV-one",
        "harbormaster:buildable",
        (
            "null",
            "\"failed\"",
            r#"{"harbormaster:buildablePHID":"PHID-HMBB-1"}"#,
        ),
    )
    .await;
    add_story(
        &test_db,
        11,
        "PHID-APPS-PhabricatorHarbormasterApplication",
        "PHID-DREV-one",
        "harbormaster:buildable",
        (
            "null",
            "\"passed\"",
            r#"{"harbormaster:buildablePHID":"PHID-HMBB-1"}"#,
        ),
    )
    .await;

    let (status, page) = get(&test_db, "/feed/").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<a href=\"/p/alice/\">alice</a> committed <a href=\"/rVCTabcdef0123456789abcdef0123456789abcdef01\">rVCTabcdef012345: \
         Add &lt;the&gt; README</a> (authored by <a href=\"/p/bob/\">bob</a>).",
        "<div class=\"feed-body\">Add &lt;the&gt; README</div>",
        "Someone &lt;someone@example.com&gt; committed <a href=\"/rVCTabcdef0123456789abcdef0123456789abcdef01\">",
        "<a href=\"/p/alice/\">alice</a> committed <a href=\"/rVCTb50ab917138e4e2fb5bf04a99d8eacf540934cdb\" \
         class=\"remarkup-object-closed\">rVCTb50ab917138e \u{267B} rVCTcd825461a99b: Rewritten</a>.",
        "<a href=\"/harbormaster/\">Harbormaster</a> failed remote builds in \
         <a href=\"/B1\">B1: Diff 1</a> for <a href=\"/D1\">D1: Open &lt;one&gt;</a>!",
    ] {
        assert!(page.contains(expected), "{expected}\n{page}");
    }
    assert!(!page.contains("Hidden"), "{page}");
    assert!(!page.contains("completed remote builds"), "{page}");
    // Only the edge and the commit story made of revision transactions
    // are left out.
    assert!(page.contains("2 stories are not shown here yet"), "{page}");

    // The commit's story, alone, and on bob's profile.
    let (status, page) = get(&test_db, &format!("/feed/{}/", chronological_key(6))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("(authored by"), "{page}");
    let (status, _) = get(&test_db, &format!("/feed/{}/", chronological_key(9))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, page) = get(&test_db, "/p/bob/").await;
    assert!(page.contains("bob</a> accepted"), "{page}");
    assert!(!page.contains("committed"), "{page}");
    test_db.drop_databases().await;
}

/// A story `id` by alice about a project, made of one `project_transaction`.
async fn add_project_story(
    test_db: &TestDatabase,
    id: u64,
    project: &str,
    kind: &str,
    (old, new, metadata): (&str, &str, &str),
) {
    let transaction = format!("PHID-XACT-PROJ-{id}");
    let data = json!({
        "objectPHID": project,
        "transactionPHIDs": {&transaction: &transaction},
    })
    .to_string();
    let key = chronological_key(id);
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_project`.project_transaction (phid, authorPHID, objectPHID, \
               viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, oldValue, \
               newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('{transaction}', 'PHID-USER-alice', '{project}', 'public', 'PHID-USER-alice', \
               NULL, 0, '{kind}', '{old}', '{new}', '{{}}', '{metadata}', 1, 1); \
             INSERT INTO `{{ns}}_feed`.feed_storydata (id, phid, chronologicalKey, storyType, \
               storyData, authorPHID, dateCreated, dateModified) VALUES \
             ({id}, 'PHID-STRY-{id}', {key}, '{STORY}', '{data}', 'PHID-USER-alice', 1, 1); \
             INSERT INTO `{{ns}}_feed`.feed_storyreference (objectPHID, chronologicalKey) \
             VALUES ('{project}', {key}), ('PHID-USER-alice', {key})"
        ))
        .await;
}

#[tokio::test]
async fn project_stories() {
    let Some(test_db) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_project`.project (name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug) VALUES \
             ('Build', 'PHID-PROJ-build', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', \
               'blue', 'k', 0, 0, 0, 'a', 0, 'k001', '{}', 'default', 'build'), \
             ('Hush', 'PHID-PROJ-hush', 'x', 1, 1, '0', 'no-one', 'users', 'users', 'fa-x', \
               'red', 'k', 0, 0, 0, 'b', 0, 'k002', '{}', 'default', 'hush')",
        )
        .await;
    // 12: alice creates Build. 13: she adds bob to it, as a list of PHIDs.
    // 14: she removes bob, as older edge dictionaries. 15: a hidden
    // project. 16: an icon, whose title is not ported. 17: a silenced
    // member, hidden from feed.
    add_project_story(
        &test_db,
        12,
        "PHID-PROJ-build",
        "core:create",
        ("null", "null", r#"{"core.create":true}"#),
    )
    .await;
    add_project_story(
        &test_db,
        13,
        "PHID-PROJ-build",
        "core:edge",
        ("[]", r#"["PHID-USER-bob"]"#, r#"{"edge:type":13}"#),
    )
    .await;
    add_project_story(
        &test_db,
        14,
        "PHID-PROJ-build",
        "core:edge",
        (
            r#"{"PHID-USER-bob":{"dst":"PHID-USER-bob","type":"13"},"PHID-USER-alice":{"dst":"PHID-USER-alice","type":"13"}}"#,
            r#"{"PHID-USER-alice":{"dst":"PHID-USER-alice","type":13}}"#,
            r#"{"edge:type":13}"#,
        ),
    )
    .await;
    add_project_story(
        &test_db,
        15,
        "PHID-PROJ-hush",
        "core:create",
        ("null", "null", r#"{"core.create":true}"#),
    )
    .await;
    add_project_story(
        &test_db,
        16,
        "PHID-PROJ-build",
        "project:icon",
        (r#""project""#, r#""group""#, "{}"),
    )
    .await;
    add_project_story(
        &test_db,
        17,
        "PHID-PROJ-build",
        "core:edge",
        ("[]", r#"["PHID-USER-bob"]"#, r#"{"edge:type":61}"#),
    )
    .await;

    let (status, page) = get(&test_db, "/feed/").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<a href=\"/p/alice/\">alice</a> created <a href=\"/tag/build/\">Build</a>.",
        "<a href=\"/p/alice/\">alice</a> added a member for <a href=\"/tag/build/\">Build</a>: \
         <a href=\"/p/bob/\">bob</a>.",
        "<a href=\"/p/alice/\">alice</a> removed a member for <a href=\"/tag/build/\">Build</a>: \
         <a href=\"/p/bob/\">bob</a>.",
    ] {
        assert!(page.contains(expected), "{expected}\n{page}");
    }
    assert!(!page.contains("Hush"), "{page}");
    // The icon, the edge and the commit story made of revision
    // transactions are left out.
    assert!(page.contains("3 stories are not shown here yet"), "{page}");
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
