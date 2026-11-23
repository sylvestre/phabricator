//! The diff and changeset pages of Differential, the revision page's
//! other diffs, raw diffs and "changes since the last action", against a
//! test database; the Ajax answers of `/differential/changeset/` against
//! what PHP answers (`tests/oracle/changeset.json`, made by
//! `tests/oracle/changeset.php`).

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use base64::Engine as _;
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::searchfox::Trees;
use phab_web::{FALLBACK_HEADER, WebState};
use serde_json::{Value, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

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

    fn searchfox_trees(&self) -> &Trees {
        static TREES: std::sync::OnceLock<Trees> = std::sync::OnceLock::new();
        TREES.get_or_init(|| Trees::parse(""))
    }
}

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex_key()).unwrap()
}

fn hex_key() -> Vec<u8> {
    (0..DIGEST_KEY_HEX.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&DIGEST_KEY_HEX[index..index + 2], 16).unwrap())
        .collect()
}

fn config() -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = json!({
        "phabricator.base-uri": "http://phab.test/",
        "storage.mysql-engine.max-size": 1_000_000,
    }) {
        config.push_layer("test", map);
    }
    config
}

/// Alice with session `alice-token`; D1 (by Alice) with diffs 7 and 9,
/// diff 8 without a revision, and the tables as later schema patches
/// made them (quickstart.sql predates them).
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
        "worker",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1), \
               ('file.integrity', '{DIGEST_KEY_HEX}', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0); \
             DROP TABLE IF EXISTS `{{ns}}_differential`.differential_changeset_parse_cache; \
             CREATE TABLE `{{ns}}_differential`.differential_changeset_parse_cache ( \
               id INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, \
               cacheIndex BINARY(12) NOT NULL, cache LONGBLOB NOT NULL, \
               dateCreated INT UNSIGNED NOT NULL, UNIQUE KEY `key_cacheIndex` (cacheIndex)); \
             DROP TABLE IF EXISTS `{{ns}}_differential`.differential_viewstate; \
             CREATE TABLE `{{ns}}_differential`.differential_viewstate ( \
               id INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, \
               viewerPHID VARBINARY(64) NOT NULL, objectPHID VARBINARY(64) NOT NULL, \
               viewState LONGTEXT NOT NULL, dateCreated INT UNSIGNED NOT NULL, \
               dateModified INT UNSIGNED NOT NULL, \
               UNIQUE KEY `key_viewer` (viewerPHID, objectPHID)); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy, repositorySlug) VALUES \
             (1, 'PHID-REPO-git', 'project', 'PROJECT', 'git', '{{}}', 1, 1, 'users', 'users', \
               'users', 'project'); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, properties, activeDiffPHID, repositoryPHID) VALUES \
             (1, 'Fix it', 'PHID-DREV-one', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'users', '{{}}', 'PHID-DIFF-nine', 'PHID-REPO-git'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, repositoryPHID, sourceControlBaseRevision, lintStatus, unitStatus, \
               lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (7, 'PHID-DIFF-seven', 1, 'PHID-USER-alice', 'PHID-REPO-git', 'abc', 0, 0, 3, 1, 1, \
               'users'), \
             (8, 'PHID-DIFF-eight', NULL, 'PHID-USER-alice', NULL, 'abc', 0, 0, 3, 1, 1, \
               'users'), \
             (9, 'PHID-DIFF-nine', 1, 'PHID-USER-alice', 'PHID-REPO-git', 'abc', 0, 0, 3, 2, 2, \
               'users'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, phid, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, newProperties, \
               addLines, delLines, dateCreated, dateModified) VALUES \
             (10, 'PHID-DCHG-ten', 7, 'src/a.c', 'src/a.c', '[]', 2, 1, '{{}}', '[]', '[]', 1, 1, 1, \
               1), \
             (11, 'PHID-DCHG-eleven', 8, NULL, 'src/b.c', '[]', 1, 1, '{{}}', '[]', '[]', 1, 0, \
               1, 1), \
             (12, 'PHID-DCHG-twelve', 9, 'src/a.c', 'src/a.c', '[]', 2, 1, '{{}}', '[]', '[]', 1, 1, \
               1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_hunk (id, changesetID, oldOffset, \
               oldLen, newOffset, newLen, dataType, dataEncoding, dataFormat, data, dateCreated, \
               dateModified) VALUES \
             (21, 10, 1, 2, 1, 2, 'text', 'utf8', 'byte', ' int x;\\n-int y = 1;\\n+int y = 2;\\n', \
               1, 1), \
             (22, 11, 0, 0, 1, 1, 'text', 'utf8', 'byte', '+int z;\\n', 1, 1), \
             (23, 12, 1, 2, 1, 2, 'text', 'utf8', 'byte', ' int x;\\n-int y = 1;\\n+int y = 3;\\n', \
               1, 1)",
            alice = session_key("alice-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    Some(test_db)
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

async fn get(test_db: &TestDatabase, path: &str) -> Response {
    let request = Request::get(path)
        .header(header::COOKIE, "phsid=alice-token")
        .body(Body::empty())
        .unwrap();
    send(test_db, request).await
}

async fn text(response: Response) -> String {
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn fallback_reason(response: &Response) -> String {
    assert_eq!(
        response.status(),
        StatusCode::MISDIRECTED_REQUEST,
        "{response:?}"
    );
    response.headers()[FALLBACK_HEADER]
        .to_str()
        .unwrap()
        .to_owned()
}

fn location(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::FOUND, "{response:?}");
    response.headers()[header::LOCATION].to_str().unwrap()
}

#[tokio::test]
async fn revisions_show_one_of_their_diffs() {
    let Some(test_db) = setup().await else {
        return;
    };
    // The newest diff by default, another one with `?id=`.
    let page = text(get(&test_db, "/D1").await).await;
    assert!(page.contains("<h2>Diff 9</h2>"), "{page}");
    assert!(
        page.contains("<span class=\"bright\"><span class=\"hl-number\">3</span>"),
        "{page}"
    );
    let page = text(get(&test_db, "/D1?id=7").await).await;
    assert!(page.contains("<h2>Diff 7</h2>"), "{page}");
    assert!(
        page.contains("<span class=\"bright\"><span class=\"hl-number\">2</span>"),
        "{page}"
    );
    // A diff of another revision, or none, is the newest one.
    let page = text(get(&test_db, "/D1?id=8").await).await;
    assert!(page.contains("<h2>Diff 9</h2>"), "{page}");
    // Comparisons are PHP's.
    let response = get(&test_db, "/D1?vs=7").await;
    assert_eq!(fallback_reason(&response), "comparisons between diffs");
    let page = text(get(&test_db, "/D1?vs=8").await).await;
    assert!(page.contains("<h2>Diff 9</h2>"), "{page}");
    let response = get(&test_db, "/D1?id=x").await;
    assert_eq!(fallback_reason(&response), "unusual diff IDs");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn new_changes_show_why_everything_is_shown() {
    let Some(test_db) = setup().await else {
        return;
    };
    // No action of Alice yet.
    let page = text(get(&test_db, "/D1/new/").await).await;
    assert!(page.contains("<strong>No Actions</strong>"), "{page}");
    assert!(
        page.contains("Showing all changes because you have never taken an action"),
        "{page}"
    );
    assert!(page.contains("<h2>Diff 9</h2>"), "{page}");

    // Alice commented after both updates.
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_transaction (phid, authorPHID, \
               objectPHID, viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, \
               oldValue, newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('PHID-XACT-DREV-1', 'PHID-USER-alice', 'PHID-DREV-one', 'public', \
               'PHID-USER-alice', NULL, 0, 'differential:update', 'null', '\"PHID-DIFF-seven\"', \
               '{}', '{}', 5, 5), \
             ('PHID-XACT-DREV-2', 'PHID-USER-alice', 'PHID-DREV-one', 'public', \
               'PHID-USER-alice', NULL, 0, 'differential:update', '\"PHID-DIFF-seven\"', \
               '\"PHID-DIFF-nine\"', '{}', '{}', 10, 10), \
             ('PHID-XACT-DREV-3', 'PHID-USER-alice', 'PHID-DREV-one', 'public', \
               'PHID-USER-alice', NULL, 0, 'core:subscribers', '[]', '[]', '{}', '{}', 20, 20)",
        )
        .await;
    let page = text(get(&test_db, "/D1/new/").await).await;
    assert!(
        page.contains("<strong>No Recent Updates</strong>"),
        "{page}"
    );

    // The diff changed since: a comparison, which PHP shows.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_transaction SET dateCreated = 30 \
             WHERE phid = 'PHID-XACT-DREV-2'",
        )
        .await;
    let response = get(&test_db, "/D1/new/").await;
    assert_eq!(fallback_reason(&response), "comparisons between diffs");

    // `id` and `vs` don't go with `/new/`.
    for path in ["/D1/new/?vs=7", "/D1/new/?id=7"] {
        assert_eq!(
            get(&test_db, path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    test_db.drop_databases().await;
}

#[tokio::test]
async fn raw_diffs_are_stored_and_redirected_to() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = get(&test_db, "/D1?download=true&id=7").await;
    let location = location(&response).to_owned();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL].to_str().unwrap(),
        "no-store"
    );
    assert_eq!(
        response.headers()[header::CONTENT_TYPE].to_str().unwrap(),
        "text/html; charset=UTF-8"
    );
    assert_eq!(
        response.headers()["referrer-policy"].to_str().unwrap(),
        "no-referrer"
    );
    let name = format!("D1.{}.diff", NOW + 86_400);
    assert!(
        location.starts_with("http://phab.test/file/data/") && location.ends_with(&name),
        "{location}"
    );

    let database = test_db.database();
    let row = phab_db::sqlx::query(&format!(
        "SELECT f.phid, f.name, f.mimeType, f.ttl, f.viewPolicy, f.authorPHID, b.data FROM {} f \
         JOIN {} b ON b.id = f.storageHandle",
        database.table("file", "file"),
        database.table("file", "file_storageblob")
    ))
    .fetch_one(database.pool())
    .await
    .unwrap();
    let column = |name: &str| phab_db::rows::opt_text(&row, name).unwrap();
    assert_eq!(column("name").as_deref(), Some(name.as_str()));
    assert_eq!(column("mimeType").as_deref(), Some("text/plain"));
    assert_eq!(column("viewPolicy").as_deref(), Some("no-one"));
    assert_eq!(column("authorPHID"), None);
    assert_eq!(
        phab_db::rows::opt_int(&row, "ttl").unwrap(),
        Some(NOW + 86_400)
    );
    let data: Vec<u8> = phab_db::sqlx::Row::try_get(&row, "data").unwrap();
    assert_eq!(
        String::from_utf8(data).unwrap(),
        "diff --git a/src/a.c b/src/a.c\n--- a/src/a.c\n+++ b/src/a.c\n@@ -1,2 +1,2 @@\n int \
         x;\n-int y = 1;\n+int y = 2;\n\n"
    );
    let file_phid = column("phid").unwrap();
    let attached = phab_db::sqlx::query(&format!(
        "SELECT objectPHID, attachmentMode FROM {} WHERE filePHID = ?",
        database.table("file", "file_attachment")
    ))
    .bind(&file_phid)
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(
        phab_db::rows::text(&attached, "objectPHID").unwrap(),
        "PHID-DREV-one"
    );
    assert_eq!(
        phab_db::rows::text(&attached, "attachmentMode").unwrap(),
        "attach"
    );

    // Without a repository, PHP makes a unified diff.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_diff SET repositoryPHID = NULL WHERE id = 9",
        )
        .await;
    let response = get(&test_db, "/D1?download=true").await;
    assert_eq!(fallback_reason(&response), "raw diffs as unified diffs");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn diffs_show_their_changesets_or_go_to_their_revision() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = get(&test_db, "/differential/diff/7/").await;
    assert_eq!(location(&response), "http://phab.test/D1?id=7");

    let page = text(get(&test_db, "/differential/diff/8/?revisionID=1").await).await;
    assert!(page.contains("<h1>Diff 8</h1>"), "{page}");
    assert!(
        page.contains("<option value=\"1\" selected>D1 Fix it</option>"),
        "{page}"
    );
    assert!(
        page.contains("<a href=\"/differential/changeset/?ref=11\">src/b.c</a>"),
        "{page}"
    );
    assert!(page.contains("int</span> z;"), "{page}");

    for (path, status) in [
        ("/differential/diff/99/", StatusCode::NOT_FOUND),
        ("/differential/diff/x/", StatusCode::NOT_FOUND),
    ] {
        assert_eq!(get(&test_db, path).await.status(), status, "{path}");
    }
    // Another user's diff without a revision, which they may not see.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_diff SET authorPHID = 'PHID-USER-bob', \
             viewPolicy = 'PHID-USER-bob' WHERE id = 8",
        )
        .await;
    assert_eq!(
        get(&test_db, "/differential/diff/8/").await.status(),
        StatusCode::FORBIDDEN
    );

    // The list of changesets.
    for path in [
        "/differential/diff/7/changesets/",
        "/differential/diff/7/changesets/query/all/",
    ] {
        let page = text(get(&test_db, path).await).await;
        assert!(
            page.contains("<a href=\"/differential/changeset/?ref=10\">src/a.c</a>"),
            "{page}"
        );
        assert!(page.contains("<td>Modified</td>"), "{page}");
    }
    let response = get(&test_db, "/differential/diff/7/changesets/query/abc123/").await;
    assert_eq!(fallback_reason(&response), "saved changeset queries");
    let response = get(&test_db, "/differential/diff/7/changesets/?diffPHIDs=x").await;
    assert_eq!(fallback_reason(&response), "a search query");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn changesets_are_shown_alone() {
    let Some(test_db) = setup().await else {
        return;
    };
    let page = text(get(&test_db, "/differential/changeset/?ref=10").await).await;
    assert!(page.contains("<h1>Changeset View</h1>"), "{page}");
    assert!(page.contains("<a href=\"/D1\">D1</a>"), "{page}");
    assert!(
        page.contains("<span class=\"bright\"><span class=\"hl-number\">2</span>"),
        "{page}"
    );

    for (path, reason) in [
        (
            "/differential/changeset/?ref=10/12",
            "comparisons between changesets",
        ),
        (
            "/differential/changeset/?ref=10&view=new",
            "raw files, which PHP may store",
        ),
        (
            "/differential/changeset/?ref=10&range=0-2",
            "ranges of a changeset",
        ),
        (
            "/differential/changeset/?ref=10&renderer=1up",
            "view state changes",
        ),
    ] {
        let response = get(&test_db, path).await;
        assert_eq!(fallback_reason(&response), reason, "{path}");
    }
    for path in [
        "/differential/changeset/",
        "/differential/changeset/?ref=99",
    ] {
        assert_eq!(
            get(&test_db, path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    test_db.drop_databases().await;
}

/// A changeset of the oracle, with its hunks and parse cache, in diff 7.
async fn insert_oracle_case(test_db: &TestDatabase, case: &Value) {
    let database = test_db.database();
    let changeset = &case["changeset"];
    let id = changeset["id"].as_i64().unwrap();
    for table in [
        "differential_changeset",
        "differential_hunk",
        "differential_changeset_parse_cache",
    ] {
        let column = match table {
            "differential_changeset" => "id",
            "differential_hunk" => "changesetID",
            _ => "1",
        };
        if column == "1" {
            phab_db::sqlx::query(&format!(
                "DELETE FROM {}",
                database.table("differential", table)
            ))
            .execute(database.pool())
            .await
            .unwrap();
        } else {
            phab_db::sqlx::query(&format!(
                "DELETE FROM {} WHERE {column} = ?",
                database.table("differential", table)
            ))
            .bind(id)
            .execute(database.pool())
            .await
            .unwrap();
        }
    }
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (id, phid, diffID, oldFile, filename, awayPaths, changeType, fileType, \
         metadata, oldProperties, newProperties, addLines, delLines, dateCreated, dateModified) \
         VALUES (?, ?, 7, ?, ?, ?, ?, 1, ?, '[]', '[]', ?, ?, 1, 1)",
        database.table("differential", "differential_changeset")
    ))
    .bind(id)
    .bind(format!("PHID-DCHG-oracle{id}"))
    .bind(changeset["oldFile"].as_str())
    .bind(changeset["filename"].as_str().unwrap())
    .bind(changeset["awayPaths"].to_string())
    .bind(changeset["changeType"].as_i64().unwrap())
    .bind(changeset["metadata"].to_string())
    .bind(changeset["addLines"].as_i64().unwrap())
    .bind(changeset["delLines"].as_i64().unwrap())
    .execute(database.pool())
    .await
    .unwrap();
    for hunk in case["hunks"].as_array().unwrap() {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (changesetID, oldOffset, oldLen, newOffset, newLen, dataType, \
             dataEncoding, dataFormat, data, dateCreated, dateModified) VALUES (?, ?, ?, ?, ?, \
             'text', ?, 'byte', ?, 1, 1)",
            database.table("differential", "differential_hunk")
        ))
        .bind(id)
        .bind(hunk["oldOffset"].as_i64().unwrap())
        .bind(hunk["oldLen"].as_i64().unwrap())
        .bind(hunk["newOffset"].as_i64().unwrap())
        .bind(hunk["newLen"].as_i64().unwrap())
        .bind(hunk["dataEncoding"].as_str())
        .bind(hunk["changes"].as_str().unwrap().as_bytes())
        .execute(database.pool())
        .await
        .unwrap();
    }
    let cache = base64::engine::general_purpose::STANDARD
        .decode(case["cache"].as_str().unwrap())
        .unwrap();
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (cacheIndex, cache, dateCreated) VALUES (?, ?, 1)",
        database.table("differential", "differential_changeset_parse_cache")
    ))
    .bind(hash::digest_for_index(id.to_string().as_bytes()))
    .bind(cache)
    .execute(database.pool())
    .await
    .unwrap();
}

fn ajax_path(reference: &str, range: Option<&str>) -> String {
    let mut path = format!(
        "/differential/changeset/?ref={reference}&device=2up&__ajax__=true&__wflow__=true&\
         __metablock__=3"
    );
    if let Some(range) = range {
        path.push_str("&range=");
        path.extend(form_urlencoded::byte_serialize(range.as_bytes()));
    }
    path
}

#[tokio::test]
async fn ajax_answers_are_php_ones() {
    let Some(test_db) = setup().await else {
        return;
    };
    let oracle: Vec<Value> = serde_json::from_str(include_str!("oracle/changeset.json")).unwrap();
    for case in &oracle {
        let name = case["name"].as_str().unwrap();
        insert_oracle_case(&test_db, case).await;
        let id = case["changeset"]["id"].as_i64().unwrap();
        let path = ajax_path(&id.to_string(), case["range"].as_str());
        let response = get(&test_db, &path).await;
        assert_eq!(response.status(), StatusCode::OK, "{name}: {response:?}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE].to_str().unwrap(),
            "text/plain; charset=UTF-8",
            "{name}"
        );
        let answer = text(response).await;
        assert_eq!(answer, case["answer"].as_str().unwrap(), "{name}");
    }
    test_db.drop_databases().await;
}

#[tokio::test]
async fn ajax_requests_rust_cannot_answer_go_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let oracle: Vec<Value> = serde_json::from_str(include_str!("oracle/changeset.json")).unwrap();
    let case = &oracle[0];
    insert_oracle_case(&test_db, case).await;
    let reason = |path: String| {
        let test_db = &test_db;
        async move { fallback_reason(&get(test_db, &path).await) }
    };

    // The unified renderer, picked by the device or the viewer.
    let path = ajax_path("101", None).replace("device=2up", "device=1up");
    assert_eq!(reason(path).await, "the unified renderer");
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_viewstate (viewerPHID, objectPHID, \
               viewState, dateCreated, dateModified) VALUES ('PHID-USER-alice', \
               'PHID-DREV-one', '{{\"changesets\":{{\"{hash}\":{{\"renderer\":{{\"101\":\
               {{\"value\":\"1up\",\"epoch\":5}}}}}}}}}}', 1, 1)",
            hash = hash::digest_for_index(b"src/main.c")
        ))
        .await;
    assert_eq!(reason(ajax_path("101", None)).await, "the unified renderer");
    test_db
        .execute("DELETE FROM `{ns}_differential`.differential_viewstate")
        .await;

    // Requests which change the view state.
    let path = format!("{}&highlight=c", ajax_path("101", None));
    assert_eq!(reason(path).await, "view state changes");

    // An inline comment on the file, here or on another diff.
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_transaction_comment (phid, \
               transactionPHID, authorPHID, viewPolicy, editPolicy, commentVersion, content, \
               contentSource, isDeleted, dateCreated, dateModified, isNewFile, lineNumber, \
               lineLength, hasReplies, attributes, changesetID, revisionPHID) VALUES \
             ('PHID-XCMT-1', NULL, 'PHID-USER-alice', 'public', 'PHID-USER-alice', 1, 'Hm', \
               '{}', 0, 30, 30, 1, 2, 0, 0, '{}', 101, 'PHID-DREV-one')",
        )
        .await;
    assert_eq!(
        reason(ajax_path("101", None)).await,
        "changesets with inline comments"
    );
    test_db
        .execute("DELETE FROM `{ns}_differential`.differential_transaction_comment")
        .await;

    // No cache: PHP renders the changeset and stores it.
    test_db
        .execute("DELETE FROM `{ns}_differential`.differential_changeset_parse_cache")
        .await;
    assert_eq!(
        reason(ajax_path("101", None)).await,
        "changesets PHP has not rendered yet"
    );

    // Posts without a valid CSRF token.
    let request = Request::post("/differential/changeset/")
        .header(header::COOKIE, "phsid=alice-token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("ref=101&__ajax__=true&__csrf__=nope"))
        .unwrap();
    assert_eq!(
        fallback_reason(&send(&test_db, request).await),
        "invalid CSRF tokens"
    );
    test_db.drop_databases().await;
}
