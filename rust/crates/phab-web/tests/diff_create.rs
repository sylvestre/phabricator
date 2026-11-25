//! The forms which create diffs from a raw diff and attach diffs to
//! revisions, against a test database: the rows they write, and what they
//! leave to PHP.

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
const CONDUIT_SOURCE: &str = r#"{"source":"conduit","params":[]}"#;
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;
const CREATE: &str = "/differential/diff/create/";
const ATTACH: &str = "/differential/revision/attach/101/to/1/";
const RAW_DIFF: &str = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";

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

/// Bob, logged in, and Alice; Bob's D1 on diff 1, and Alice's D2, which
/// only she may edit; Bob's unattached diff 101 in the Git repository
/// `rGIT`, with a file and its local commit.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(DATABASES).await?;
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
             ('PHID-USER-bob', 'web', '{bob}', 1699000000, {expires}, 'PHID-SSSN-1', 0, NULL); \
             INSERT INTO `{{ns}}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-git', 'Git', 'GIT', 'git', '{{}}', 1, 1, 'users', 'admin', 'admin'); \
             INSERT INTO `{{ns}}_repository`.repository_path (id, path, pathHash) VALUES \
             (7, '/', MD5('/')); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-review', 'Old summary.', 'Ran it.', \
               'PHID-USER-bob', 1, 1, '{{}}', 'k', 'users', 'users', NULL, '{{}}', \
               'PHID-DIFF-1'), \
             (2, 'Other', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'users', 'PHID-USER-alice', NULL, '{{}}', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, repositoryPHID, sourceControlSystem, sourceControlPath, lintStatus, \
               unitStatus, lineCount, dateCreated, dateModified, viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-bob', NULL, 'git', '/', 0, 0, 1, 1, 1, \
               'PHID-USER-bob'), \
             (2, 'PHID-DIFF-2', 2, 'PHID-USER-alice', NULL, 'git', '/', 0, 0, 1, 1, 1, \
               'PHID-USER-alice'), \
             (101, 'PHID-DIFF-new', NULL, 'PHID-USER-bob', 'PHID-REPO-git', 'git', '/', 0, 0, \
               3, 1, 1, 'PHID-USER-bob'); \
             INSERT INTO `{{ns}}_differential`.differential_changeset (id, diffID, oldFile, \
               filename, awayPaths, changeType, fileType, metadata, oldProperties, \
               newProperties, addLines, delLines, dateCreated, dateModified, phid) VALUES \
             (11, 101, 'README.md', 'README.md', '[]', 2, 1, '{{}}', '{{}}', '{{}}', 2, 1, 1, 1, \
               'PHID-DCNG-11'); \
             INSERT INTO `{{ns}}_differential`.differential_diffproperty (diffID, name, data, \
               dateCreated, dateModified) VALUES \
             (101, 'local:commits', '{{\"aaa\":{{\"commit\":\"aaa\",\"tree\":\"ttt\"}}}}', 1, 1)",
            secret = ACCOUNT_SECRET,
            bob = session_key("bob-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    Some(test_db)
}

/// A CSRF token of Bob's session.
async fn csrf_token(test_db: &TestDatabase) -> String {
    let Outcome::Done(key) = csrf::load_key(test_db.database()).await.unwrap() else {
        panic!("no CSRF key");
    };
    let secret = CsrfSecret::for_session(
        &Phid::new("PHID-USER-bob"),
        ACCOUNT_SECRET,
        &session_key("bob-token"),
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

async fn body(response: Response) -> String {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&body).into_owned()
}

async fn get(test_db: &TestDatabase, path: &str) -> Response {
    let request = Request::get(path)
        .header(header::COOKIE, BOB)
        .header(header::HOST, "phabricator.test")
        .body(Body::empty())
        .unwrap();
    send(test_db, request).await
}

/// Posts `fields` to `path` as a form, with Bob's CSRF token unless the
/// fields have one.
async fn post(test_db: &TestDatabase, path: &str, fields: &[(&str, &str)]) -> Response {
    let token = csrf_token(test_db).await;
    let mut all: Vec<(&str, &str)> = vec![("__form__", "1")];
    if !fields.iter().any(|(key, _)| *key == "__csrf__") {
        all.push(("__csrf__", &token));
    }
    all.extend_from_slice(fields);
    let request = Request::post(path)
        .header(header::COOKIE, BOB)
        .header(header::HOST, "phabricator.test")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            form_urlencoded::Serializer::new(String::new())
                .extend_pairs(all)
                .finish(),
        ))
        .unwrap();
    send(test_db, request).await
}

/// A post which goes to PHP: its path, its fields and why.
type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);

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

/// The diffs, from the first new one.
async fn new_diffs(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', id, IFNULL(repositoryPHID, '-'), creationMethod, viewPolicy) \
           AS line FROM `{ns}_differential`.differential_diff WHERE id > 101 ORDER BY id",
    )
    .await
}

/// The transactions of the new diffs: type, values and content source.
async fn diff_transactions(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', d.id, x.transactionType, x.oldValue, x.newValue, \
           x.contentSource) AS line \
         FROM `{ns}_differential`.differential_difftransaction x \
         JOIN `{ns}_differential`.differential_diff d ON d.phid = x.objectPHID ORDER BY x.id",
    )
    .await
}

#[tokio::test]
async fn the_forms_show_the_diff_to_create() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = get(&test_db, CREATE).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;
    for expected in [
        "<h1>Create Diff</h1>",
        r#"<textarea id="raw-diff" name="diff" rows="24"></textarea>"#,
        r#"<option value="PHID-USER-bob" selected>Only bob (Bob)</option>"#,
        r#"<option value="users">All Users</option>"#,
        "landable using",
    ] {
        assert!(page.contains(expected), "{expected} in {page}");
    }
    assert!(!page.contains("Public (No Login Required)"), "{page}");

    // A repository typed in the Rust input comes back as a token.
    let response = post(
        &test_db,
        CREATE,
        &[
            ("diff", RAW_DIFF),
            ("viewPolicy", "users"),
            ("rust.add.repository", "rGIT"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;
    for expected in [
        r#"name="differential:repository[]" value="PHID-REPO-git" checked> rGIT Git"#,
        r#"<option value="users" selected>All Users</option>"#,
        "+b\n</textarea>",
    ] {
        assert!(page.contains(expected), "{expected} in {page}");
    }
    let response = post(&test_db, CREATE, &[("rust.add.repository", "nope")]).await;
    assert!(
        body(response)
            .await
            .contains("No repository you can see is named nope.")
    );
    // An empty diff is PHP's error on the form.
    let response = post(&test_db, CREATE, &[("diff", "")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body(response)
            .await
            .contains("You can not create an empty diff.")
    );

    // The update form starts with the revision's repository.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_revision SET repositoryPHID = \
             'PHID-REPO-git' WHERE id = 1",
        )
        .await;
    let response = get(&test_db, "/differential/revision/update/1/").await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;
    for expected in [
        "<h1>Update Diff</h1>",
        r#"<a href="/D1">D1: Open</a>"#,
        r#"value="PHID-REPO-git" checked"#,
        r#"action="/differential/revision/update/1/""#,
    ] {
        assert!(page.contains(expected), "{expected} in {page}");
    }
    assert_eq!(new_diffs(&test_db).await, Vec::<String>::new());
}

#[tokio::test]
async fn diffs_are_created_as_php_creates_them() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = post(
        &test_db,
        CREATE,
        &[
            ("diff", &RAW_DIFF.replace('\n', "\r\n")),
            ("differential:repository[]", "PHID-REPO-git"),
            ("viewPolicy", "users"),
        ],
    )
    .await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/differential/diff/102/"
    );
    // Without a view policy, the diff keeps its author's.
    let response = post(
        &test_db,
        "/differential/revision/update/1/",
        &[("diff", RAW_DIFF)],
    )
    .await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/differential/diff/103/?revisionID=1"
    );
    assert_eq!(
        new_diffs(&test_db).await,
        vec!["102 PHID-REPO-git web users", "103 - web PHID-USER-bob"]
    );
    assert_eq!(
        diff_transactions(&test_db).await,
        vec![
            format!("102 differential:diff:create null true {CONDUIT_SOURCE}"),
            format!("102 core:view-policy null \"users\" {CONDUIT_SOURCE}"),
            format!("103 differential:diff:create null true {CONDUIT_SOURCE}"),
        ]
    );
    // The line endings were PHP's `getStr()`'s.
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', filename, addLines, delLines) AS line \
             FROM `{ns}_differential`.differential_changeset WHERE diffID = 102"
        )
        .await,
        vec!["x 1 1"]
    );
}

#[tokio::test]
async fn what_the_diff_forms_do_not_write_goes_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let cases: &[Case] = &[
        (
            CREATE,
            &[
                ("diff", RAW_DIFF),
                ("viewPolicy", "obj.differential.author"),
            ],
            "object and custom view policies",
        ),
        (
            CREATE,
            &[("diff", RAW_DIFF), ("__csrf__", "bad")],
            "invalid CSRF tokens",
        ),
        (
            CREATE,
            &[("diff", RAW_DIFF), ("__wflow__", "1")],
            "a workflow request",
        ),
        (
            CREATE,
            &[("diff", "Index: x\n===\n")],
            "diffs which are not Git diffs",
        ),
        (
            CREATE,
            &[
                ("diff", RAW_DIFF),
                ("differential:repository[]", "PHID-REPO-nope"),
            ],
            "diffs PHP refuses",
        ),
        (
            "/differential/revision/update/2/",
            &[("diff", RAW_DIFF)],
            "revisions the viewer can't edit",
        ),
    ];
    for (path, fields, reason) in cases {
        let response = post(&test_db, path, fields).await;
        let actual = fallback_reason(&response);
        // The parser's own reasons vary with the diff.
        if *reason == "diffs which are not Git diffs" {
            assert_ne!(actual, "");
        } else {
            assert_eq!(actual, *reason, "{fields:?}");
        }
    }
    // An uploaded file is a multipart post, which Rust doesn't read.
    let request = Request::post(CREATE)
        .header(header::COOKIE, BOB)
        .header(header::HOST, "phabricator.test")
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=x")
        .body(Body::from("--x--\r\n"))
        .unwrap();
    assert_eq!(
        fallback_reason(&send(&test_db, request).await),
        "requests which aren't form posts"
    );
    assert_eq!(
        get(&test_db, "/differential/revision/update/9/")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(new_diffs(&test_db).await, Vec::<String>::new());
}

/// The fields PHP's form posts to update D1 with diff 101, with `changes`
/// added.
fn attach_fields<'a>(changes: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("editEngine", "true"),
        ("update[]", "PHID-DIFF-new"),
        ("update.comment", ""),
        ("title", "Open"),
        ("author[0]", "PHID-USER-bob"),
        ("summary", "Old summary."),
        ("summary_metadata", "{}"),
        ("testPlan", "Ran it."),
        ("testPlan_metadata", "{}"),
        ("policy.view", "users"),
        ("policy.edit", "users"),
        ("differential:bugzilla-bug-id", ""),
    ];
    for &(key, value) in changes {
        match fields.iter_mut().find(|(known, _)| *known == key) {
            Some(field) => field.1 = value,
            None => fields.push((key, value)),
        }
    }
    fields
}

#[tokio::test]
async fn revisions_are_updated_with_a_diff_as_php_form_does() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = get(&test_db, ATTACH).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;
    for expected in [
        "Update Revision D1: Open",
        r#"<input type="hidden" name="update[]" value="PHID-DIFF-new">"#,
        r#"name="update.comment""#,
        r#"action="/differential/revision/attach/101/to/1/""#,
    ] {
        assert!(page.contains(expected), "{expected} in {page}");
    }

    let response = post(
        &test_db,
        ATTACH,
        &attach_fields(&[("update.comment", "Rebased."), ("title", "Open, rebased")]),
    )
    .await;
    assert_eq!(location(&response), "http://phabricator.test/D1");
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', transactionType, contentSource) AS line \
             FROM `{ns}_differential`.differential_transaction ORDER BY id"
        )
        .await,
        vec![
            format!("differential:update {WEB_SOURCE}"),
            format!("differential.revision.title {WEB_SOURCE}"),
            format!("core:customfield {WEB_SOURCE}"),
            format!("core:comment {WEB_SOURCE}"),
        ]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', IFNULL(revisionID, '-'), \
               (SELECT activeDiffPHID FROM `{ns}_differential`.differential_revision \
                 WHERE id = 1)) AS line \
             FROM `{ns}_differential`.differential_diff WHERE id = 101"
        )
        .await,
        vec!["1 PHID-DIFF-new"]
    );
}

#[tokio::test]
async fn what_the_attach_form_does_not_write_goes_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let cases: &[Case] = &[
        (
            ATTACH,
            &[("update[]", "PHID-DIFF-2")],
            "updates with another diff",
        ),
        (
            ATTACH,
            &[("author[0]", "PHID-USER-alice")],
            "changes to the author, repository or policies",
        ),
        (
            "/differential/revision/attach/1/to/1/",
            &[],
            "diffs already attached",
        ),
        (
            "/differential/revision/attach/101/to/2/",
            &[],
            "revisions the viewer can't edit",
        ),
        (
            "/differential/revision/attach/101/to/",
            &[],
            "new revisions from a diff",
        ),
        (
            "/differential/revision/attach/101/to/1/edit/",
            &[],
            "edit actions of revision forms",
        ),
    ];
    for (path, changes, reason) in cases {
        let response = post(&test_db, path, &attach_fields(changes)).await;
        assert_eq!(fallback_reason(&response), *reason, "{path} {changes:?}");
    }
    assert_eq!(
        get(&test_db, "/differential/revision/attach/999/to/1/")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT transactionType AS line FROM `{ns}_differential`.differential_transaction"
        )
        .await,
        Vec::<String>::new()
    );
}
