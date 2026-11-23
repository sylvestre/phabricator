//! The edit form of revisions against a test database: the form, the rows
//! the Rust editor writes for its posts, and what it leaves to PHP.

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
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;
const PATH: &str = "/differential/revision/edit/1/";

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

/// Alice's revision D1, which Bob reviews and edits; Dave and the "Build"
/// project are there to be added.
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
             ('PHID-USER-bob', 'web', '{bob}', 1699000000, {expires}, 'PHID-SSSN-1', 0, NULL); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-review', 'Old summary.', 'Ran it.', \
               'PHID-USER-alice', 1, 1, '{{}}', 'k', 'users', 'users', NULL, '{{}}', \
               'PHID-DIFF-1'); \
             INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, \
               reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-bob', 'added', 1, 1, '{{}}'); \
             INSERT INTO `{{ns}}_project`.project (name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug) VALUES \
             ('Build', 'PHID-PROJ-build', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', \
               'blue', 'k', 0, 0, 0, 'a', 0, 'k001', '{{}}', 'default', 'build'); \
             INSERT INTO `{{ns}}_project`.project_slug (slug, projectPHID, dateCreated, \
               dateModified) VALUES ('build', 'PHID-PROJ-build', 1, 1)",
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

async fn get(test_db: &TestDatabase) -> Response {
    let request = Request::get(PATH)
        .header(header::COOKIE, BOB)
        .header(header::HOST, "phabricator.test")
        .body(Body::empty())
        .unwrap();
    send(test_db, request).await
}

/// The fields PHP's form posts for D1 as it is, with `changes`, which
/// replace the fields of the same name (`<none>` removes one).
fn fields(token: &str, changes: &[(&str, &str)]) -> String {
    let mut fields: Vec<(&str, &str)> = vec![
        ("__form__", "1"),
        ("__csrf__", token),
        ("editEngine", "true"),
        ("title", "Open"),
        ("author[0]", "PHID-USER-alice"),
        ("summary", "Old summary."),
        ("summary_metadata", "{}"),
        ("testPlan", "Ran it."),
        ("testPlan_metadata", "{}"),
        ("reviewers[0]", "PHID-USER-bob"),
        ("reviewers.initial[]", "PHID-USER-bob"),
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
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().filter(|(_, value)| *value != "<none>"))
        .finish()
}

async fn edit(test_db: &TestDatabase, changes: &[(&str, &str)]) -> Response {
    let token = csrf_token(test_db).await;
    let request = Request::post(PATH)
        .header(header::COOKIE, BOB)
        .header(header::HOST, "phabricator.test")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(fields(&token, changes)))
        .unwrap();
    send(test_db, request).await
}

fn fallback_reason(response: &Response) -> &str {
    assert_eq!(
        response.status(),
        StatusCode::MISDIRECTED_REQUEST,
        "{response:?}"
    );
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

#[tokio::test]
async fn the_form_shows_the_revision() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = get(&test_db).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;
    for expected in [
        "Edit Revision D1: Open",
        r#"name="title" value="Open""#,
        "Old summary.</textarea>",
        r#"<input type="checkbox" name="reviewers[]" value="PHID-USER-bob" checked> bob"#,
        r#"<input type="hidden" name="reviewers.initial[]" value="PHID-USER-bob">"#,
        r#"<input type="hidden" name="author[]" value="PHID-USER-alice">"#,
        r#"<input type="hidden" name="policy.view" value="users">"#,
        r#"name="differential:bugzilla-bug-id" value="""#,
    ] {
        assert!(page.contains(expected), "{expected} in {page}");
    }

    // Names typed in the Rust inputs come back as tokens, unwritten.
    let response = edit(
        &test_db,
        &[
            ("rust.add.reviewers", "dave!"),
            ("rust.add.projectPHIDs", "#build"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;
    for expected in [
        r#"name="reviewers[]" value="blocking(PHID-USER-dave)" checked> dave (blocking)"#,
        r#"name="projectPHIDs[]" value="PHID-PROJ-build" checked>"#,
    ] {
        assert!(page.contains(expected), "{expected} in {page}");
    }
    // An unknown name is said on the form.
    let response = edit(&test_db, &[("rust.add.subscriberPHIDs", "nobody")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body(response)
            .await
            .contains("No user or project is named nobody.")
    );
    assert_eq!(transactions(&test_db).await, Vec::<String>::new());
}

#[tokio::test]
async fn edits_are_written_as_php_form_writes_them() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = edit(
        &test_db,
        &[
            ("title", "Renamed"),
            ("summary", "New\r\nsummary."),
            ("reviewers[1]", "PHID-USER-dave"),
            ("projectPHIDs[0]", "PHID-PROJ-build"),
            ("subscriberPHIDs[0]", "PHID-USER-dave"),
        ],
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
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        cookie.starts_with("phfrm=edit-engine%2Fdifferential.revision"),
        "{cookie}"
    );
    assert_eq!(
        transactions(&test_db).await,
        vec![
            format!(r#"differential.revision.title {WEB_SOURCE} {{"core.groupID":"G"}}"#),
            format!(
                r#"differential.revision.summary {WEB_SOURCE} {{"remarkup.control":[],"core.groupID":"G"}}"#
            ),
            format!(r#"differential.revision.reviewers {WEB_SOURCE} {{"core.groupID":"G"}}"#),
            format!(r#"core:edge {WEB_SOURCE} {{"edge:type":41,"core.groupID":"G"}}"#),
            format!(r#"core:subscribers {WEB_SOURCE} {{"core.groupID":"G"}}"#),
            format!(
                r#"core:customfield {WEB_SOURCE} {{"customfield:key":"differential:bugzilla-bug-id","core.groupID":"G"}}"#
            ),
        ]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', title, summary) AS line \
             FROM `{ns}_differential`.differential_revision"
        )
        .await,
        vec!["Renamed New\nsummary."]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT reviewerPHID AS line FROM `{ns}_differential`.differential_reviewer \
             ORDER BY reviewerPHID"
        )
        .await,
        vec!["PHID-USER-bob", "PHID-USER-dave"]
    );
}

#[tokio::test]
async fn what_rust_does_not_write_goes_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    // The bug ID's empty value has an effect without a stored one.
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_differential`.differential_customfieldstorage (objectPHID, \
               fieldIndex, fieldValue) VALUES ('PHID-DREV-1', '{}', '')",
            hash::digest_for_index(b"differential:bugzilla-bug-id")
        ))
        .await;
    let cases: &[(&[(&str, &str)], &str)] = &[
        (&[], "edits without effect"),
        (
            &[("author[0]", "PHID-USER-bob")],
            "changes to the author, repository or policies",
        ),
        (
            &[("policy.edit", "admin")],
            "changes to the author, repository or policies",
        ),
        (&[("summary", "<none>")], "form posts without every field"),
        (&[("title", "")], "empty titles"),
        (&[("testPlan", "")], "empty test plans"),
        (
            &[(
                "summary_metadata",
                r#"{"attachedFilePHIDs":["PHID-FILE-1"]}"#,
            )],
            "texts with attached files",
        ),
        (
            &[("summary", "Reviewers: dave")],
            "texts which read as commit message fields",
        ),
        (
            &[("reviewers[0]", "blocking(PHID-USER-bob)")],
            "list changes which add and remove",
        ),
        (
            &[("editEngine", "<none>")],
            "posts which aren't edit form posts",
        ),
        (&[("__csrf__", "bad")], "invalid CSRF tokens"),
        (
            &[("__ajax__", "1"), ("title", "Ajax")],
            "a workflow request",
        ),
    ];
    for (changes, reason) in cases {
        let response = edit(&test_db, changes).await;
        assert_eq!(fallback_reason(&response), *reason, "{changes:?}");
    }
    assert_eq!(transactions(&test_db).await, Vec::<String>::new());
    // Bob may not edit Alice's revision any more: PHP explains its policy.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_revision SET editPolicy = 'PHID-USER-alice'",
        )
        .await;
    assert_eq!(
        fallback_reason(&get(&test_db).await),
        "revisions the viewer can't edit"
    );
}
