//! The project pages and writes of the web interface against a test
//! database: the edit form, joining, leaving, watching, the project, people,
//! dashboard and panel lists, the "Manage" page of users and favorites.

// Few databases, many scenarios each: creating schemas is slow.
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
use serde_json::{Map, Value, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
/// A user's `accountSecret`, which fills its `binary(64)` column.
const ACCOUNT_SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
const BOB: &str = "phsid=bob-token; phab_ui=rust";
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;

const DATABASES: &[&str] = &[
    "project",
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "herald",
    "worker",
    "search",
    "dashboard",
    "differential",
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

/// Project 1 (Build Reviewers, Carol a member), project 2 (Locked Team,
/// which Bob can't edit, Bob a member) and a milestone of project 1; Bob
/// has a session, Alice authored a dashboard and a panel.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (1, 'PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, '{secret}', 0, 0), \
             (2, 'PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, '{secret}', 0, 0), \
             (3, 'PHID-USER-carol', 'carol', 'Carol', 1, 1, 'c', 0, 1, 0, 1, 1, '{secret}', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial, \
                highSecurityUntil) VALUES \
             ('PHID-USER-bob', 'web', '{bob}', 1699000000, {expires}, 'PHID-SSSN-1', 0, NULL); \
             INSERT INTO `{{ns}}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, isMembershipLocked, icon, \
               color, mailKey, primarySlug, parentProjectPHID, milestoneNumber, hasWorkboard, \
               hasMilestones, hasSubprojects, projectPath, projectDepth, projectPathKey, \
               properties, subtype) VALUES \
             (1, 'Build Reviewers', 'PHID-PROJ-build', 'PHID-USER-alice', 5, 5, '0', 'users', \
               'users', 'users', 0, 'project', 'blue', 'k', 'build_reviewers', NULL, NULL, 0, 1, \
               0, 'aaaa', 0, 'aaaa', '[]', 'default'), \
             (2, 'Locked Team', 'PHID-PROJ-locked', 'PHID-USER-alice', 5, 5, '0', 'users', \
               'admin', 'users', 1, 'group', 'red', 'k', 'locked_team', NULL, NULL, 0, 0, 0, \
               'bbbb', 0, 'bbbb', '[]', 'default'), \
             (3, 'Sprint 1', 'PHID-PROJ-sprint', 'PHID-USER-alice', 5, 5, '0', 'users', \
               'users', 'users', 0, 'timeline', 'blue', 'k', 'sprint_1', 'PHID-PROJ-build', 1, \
               0, 0, 0, 'aaaacccc', 1, 'cccc', '[]', 'default'); \
             INSERT INTO `{{ns}}_project`.project_slug (id, projectPHID, slug, dateCreated, \
               dateModified) VALUES \
             (1, 'PHID-PROJ-build', 'build_reviewers', 5, 5), \
             (2, 'PHID-PROJ-build', 'builders', 5, 5), \
             (3, 'PHID-PROJ-locked', 'locked_team', 5, 5); \
             INSERT INTO `{{ns}}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-PROJ-build', 13, 'PHID-USER-carol', 5, 0), \
             ('PHID-PROJ-build', 60, 'PHID-USER-carol', 5, 0), \
             ('PHID-PROJ-locked', 13, 'PHID-USER-bob', 5, 0), \
             ('PHID-PROJ-locked', 60, 'PHID-USER-bob', 5, 0); \
             INSERT INTO `{{ns}}_user`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-USER-carol', 14, 'PHID-PROJ-build', 5, 0), \
             ('PHID-USER-bob', 14, 'PHID-PROJ-locked', 5, 0); \
             INSERT INTO `{{ns}}_user`.user_transaction (phid, authorPHID, objectPHID, \
               viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, oldValue, \
               newValue, contentSource, metadata, dateCreated, dateModified) VALUES \
             ('PHID-XACT-USER-1', 'PHID-USER-alice', 'PHID-USER-carol', 'public', \
               'PHID-USER-alice', NULL, 0, 'user.disable', 'false', 'true', '{{}}', '{{}}', 9, 9); \
             INSERT INTO `{{ns}}_dashboard`.dashboard (id, phid, name, viewPolicy, editPolicy, \
               status, authorPHID, icon, layoutConfig, dateCreated, dateModified) VALUES \
             (1, 'PHID-DSHB-1', 'Team Board', 'users', 'users', 'active', 'PHID-USER-alice', \
               'fa-dashboard', '{{}}', 5, 5), \
             (2, 'PHID-DSHB-2', 'Old Board', 'users', 'users', 'archived', 'PHID-USER-bob', \
               'fa-dashboard', '{{}}', 5, 5); \
             INSERT INTO `{{ns}}_dashboard`.dashboard_panel (id, phid, name, viewPolicy, \
               editPolicy, panelType, authorPHID, isArchived, properties, dateCreated, \
               dateModified) VALUES \
             (1, 'PHID-DSHP-1', 'Welcome', 'users', 'users', 'text', 'PHID-USER-alice', 0, \
               '{{\"text\":\"Hello **team**\"}}', 5, 5)",
            secret = ACCOUNT_SECRET,
            bob = session_key("bob-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    Some(test_db)
}

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

/// Posts a form as Bob, with `__form__` and a valid CSRF token.
async fn post(test_db: &TestDatabase, path: &str, pairs: &[(&str, &str)]) -> Response {
    let token = csrf_token(test_db).await;
    let mut all = vec![("__form__", "1"), ("__csrf__", token.as_str())];
    all.extend_from_slice(pairs);
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(&all)
        .finish();
    let request = Request::post(path)
        .header(header::COOKIE, BOB)
        .header(header::HOST, "phabricator.test")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(test_db, request).await
}

/// Gets a page as Bob: its status and body.
async fn get(test_db: &TestDatabase, path: &str) -> (StatusCode, String) {
    let request = Request::get(path)
        .header(header::COOKIE, BOB)
        .header(header::HOST, "phabricator.test")
        .body(Body::empty())
        .unwrap();
    let response = send(test_db, request).await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn fallback_reason(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
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

/// The transactions of a project: type, values, metadata without the
/// group ID, content source.
async fn transactions(test_db: &TestDatabase, project: &str) -> Vec<String> {
    lines(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' ', transactionType, oldValue, newValue, \
               REPLACE(metadata, SUBSTRING(metadata, LOCATE('\"core.groupID\":\"', metadata) + \
               16, 32), 'G'), authorPHID, contentSource) AS line \
             FROM `{{ns}}_project`.project_transaction WHERE objectPHID = '{project}' ORDER BY id"
        ),
    )
    .await
}

async fn edges(test_db: &TestDatabase, database: &str, edge_type: i64) -> Vec<String> {
    lines(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' ', src, dst) AS line FROM `{{ns}}_{database}`.edge \
             WHERE type = {edge_type} ORDER BY src, dst"
        ),
    )
    .await
}

/// The `via` stamp of the last publish task.
async fn via_stamp(test_db: &TestDatabase) -> Value {
    let data = lines(
        test_db,
        "SELECT data AS line FROM `{ns}_worker`.worker_taskdata ORDER BY id DESC LIMIT 1",
    )
    .await;
    let data: Value = serde_json::from_str(&data[0]).unwrap();
    data["state"]["mailStamps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stamp| stamp["key"] == "via")
        .unwrap()["value"]
        .clone()
}

/// The fields of the edit form of project 1, unchanged, with `changes`.
fn edit_form<'a>(changes: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("editEngine", "true"),
        ("name", "Build Reviewers"),
        ("std:project:internal:description", ""),
        ("icon", "project"),
        ("color", "blue"),
        ("slugs", "builders"),
        ("policy.view", "users"),
        ("policy.edit", "users"),
        ("policy.join", "users"),
    ];
    for (key, value) in changes {
        let field = fields
            .iter_mut()
            .find(|(known, _)| known == key)
            .expect("a field of the form");
        field.1 = value;
    }
    fields
}

#[tokio::test]
async fn joining_and_leaving_are_written_from_the_web() {
    let Some(test_db) = setup().await else {
        return;
    };
    let (status, body) = get(&test_db, "/project/update/1/join/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Join Project"), "{body}");
    // Bob can't edit the locked project, so he can't leave it.
    let (status, body) = get(&test_db, "/project/update/2/leave/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Membership Locked"), "{body}");

    let response = post(&test_db, "/project/update/1/join/", &[]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/members/1/"
    );
    assert_eq!(
        transactions(&test_db, "PHID-PROJ-build").await,
        vec![format!(
            r#"core:edge [] ["PHID-USER-bob"] {{"edge:type":13,"core.groupID":"G"}} PHID-USER-bob {WEB_SOURCE}"#
        )]
    );
    assert_eq!(
        edges(&test_db, "project", 60).await,
        [
            "PHID-PROJ-build PHID-USER-bob",
            "PHID-PROJ-build PHID-USER-carol",
            "PHID-PROJ-locked PHID-USER-bob",
        ]
    );
    assert_eq!(
        edges(&test_db, "user", 14).await,
        [
            "PHID-USER-bob PHID-PROJ-build",
            "PHID-USER-bob PHID-PROJ-locked",
            "PHID-USER-carol PHID-PROJ-build",
        ]
    );
    assert_eq!(via_stamp(&test_db).await, json!("web"));

    // Joining again has no effect, which PHP decides about.
    let response = post(&test_db, "/project/update/1/join/", &[]).await;
    assert_eq!(fallback_reason(&response), "edits without effect");
    // Leaving a locked project needs CAN_EDIT.
    let response = post(&test_db, "/project/update/2/leave/", &[]).await;
    assert_eq!(fallback_reason(&response), "edits the actor may not make");
    // Milestones have no members of their own.
    let response = post(&test_db, "/project/update/3/join/", &[]).await;
    assert_eq!(
        fallback_reason(&response),
        "members of parents and milestones"
    );
    let response = post(
        &test_db,
        "/project/update/1/leave/",
        &[("__ajax__", "true"), ("__wflow__", "true")],
    )
    .await;
    assert_eq!(fallback_reason(&response), "a workflow request");

    let response = post(&test_db, "/project/update/1/leave/", &[]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/members/1/"
    );
    assert_eq!(
        edges(&test_db, "project", 13).await,
        [
            "PHID-PROJ-build PHID-USER-carol",
            "PHID-PROJ-locked PHID-USER-bob",
        ]
    );
}

#[tokio::test]
async fn watching_is_written_from_the_web() {
    let Some(test_db) = setup().await else {
        return;
    };
    let (status, body) = get(&test_db, "/project/watch/1/?via=profile").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"<input type="hidden" name="via" value="profile">"#),
        "{body}"
    );

    // Without `__dialog__`, PHP shows the dialog again.
    let response = post(&test_db, "/project/watch/1/", &[]).await;
    assert_eq!(
        fallback_reason(&response),
        "posts which aren't dialog posts"
    );
    let response = post(
        &test_db,
        "/project/watch/1/",
        &[("__dialog__", "1"), ("via", "profile")],
    )
    .await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/profile/1/"
    );
    assert_eq!(
        transactions(&test_db, "PHID-PROJ-build").await,
        vec![format!(
            r#"core:edge [] ["PHID-USER-bob"] {{"edge:type":47,"core.groupID":"G"}} PHID-USER-bob {WEB_SOURCE}"#
        )]
    );
    assert_eq!(
        edges(&test_db, "project", 47).await,
        ["PHID-PROJ-build PHID-USER-bob"]
    );
    assert_eq!(
        edges(&test_db, "user", 48).await,
        ["PHID-USER-bob PHID-PROJ-build"]
    );
    // Watching doesn't change the members.
    assert_eq!(
        edges(&test_db, "project", 60).await,
        [
            "PHID-PROJ-build PHID-USER-carol",
            "PHID-PROJ-locked PHID-USER-bob",
        ]
    );

    // Watching needs no capability: Bob can't edit the locked project.
    let response = post(&test_db, "/project/watch/2/", &[("__dialog__", "1")]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/members/2/"
    );
    let response = post(&test_db, "/project/unwatch/1/", &[("__dialog__", "1")]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/members/1/"
    );
    assert_eq!(
        edges(&test_db, "project", 47).await,
        ["PHID-PROJ-locked PHID-USER-bob"]
    );
    let response = post(&test_db, "/project/unwatch/1/", &[("__dialog__", "1")]).await;
    assert_eq!(fallback_reason(&response), "edits without effect");
    let response = post(&test_db, "/project/watch/3/", &[("__dialog__", "1")]).await;
    assert_eq!(
        fallback_reason(&response),
        "watching subprojects and milestones"
    );
}

#[tokio::test]
async fn the_edit_form_writes_what_changed() {
    let Some(test_db) = setup().await else {
        return;
    };
    let (status, body) = get(&test_db, "/project/edit/1/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"value="Build Reviewers""#), "{body}");
    assert!(body.contains(r#"value="builders""#), "{body}");
    assert!(
        body.contains(r#"name="std:project:internal:description""#),
        "{body}"
    );
    // Bob can't edit the locked project: PHP explains why.
    let (status, _) = get(&test_db, "/project/edit/2/").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
    let (status, _) = get(&test_db, "/project/edit/1/?parent=2").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);

    for (changes, reason) in [
        (
            vec![("std:project:internal:description", "Builds.")],
            "description changes",
        ),
        (vec![("policy.edit", "admin")], "policy changes"),
        (vec![("icon", "nope")], "icons the form doesn't offer"),
        (vec![], "edits without effect"),
        (vec![("name", "Locked Team")], "edits PHP refuses"),
    ] {
        let response = post(&test_db, "/project/edit/1/", &edit_form(&changes)).await;
        assert_eq!(fallback_reason(&response), reason, "{changes:?}");
    }
    let mut partial = edit_form(&[("name", "Build")]);
    partial.retain(|(key, _)| *key != "policy.join");
    let response = post(&test_db, "/project/edit/1/", &partial).await;
    assert_eq!(fallback_reason(&response), "form posts without every field");
    assert_eq!(
        transactions(&test_db, "PHID-PROJ-build").await,
        Vec::<String>::new()
    );

    let response = post(
        &test_db,
        "/project/edit/1/",
        &edit_form(&[
            ("name", "Build Team"),
            ("color", "red"),
            ("slugs", "builders, build"),
        ]),
    )
    .await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/manage/1/"
    );
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        cookie.starts_with("phfrm=edit-engine%2Fprojects.project"),
        "{cookie}"
    );
    assert_eq!(
        transactions(&test_db, "PHID-PROJ-build").await,
        [
            format!(
                r#"project:name "Build Reviewers" "Build Team" {{"core.groupID":"G"}} PHID-USER-bob {WEB_SOURCE}"#
            ),
            format!(
                r#"project:color "blue" "red" {{"core.groupID":"G"}} PHID-USER-bob {WEB_SOURCE}"#
            ),
            format!(
                r#"project:slugs ["builders"] ["builders","build"] {{"core.groupID":"G"}} PHID-USER-bob {WEB_SOURCE}"#
            ),
        ]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', name, primarySlug, color) AS line \
             FROM `{ns}_project`.project WHERE id = 1"
        )
        .await,
        ["Build Team build_team red"]
    );
    assert_eq!(via_stamp(&test_db).await, json!("web"));
}

#[tokio::test]
async fn lists_show_the_builtin_queries() {
    let Some(test_db) = setup().await else {
        return;
    };
    // Joined: Bob is a member of the locked project only.
    let (status, body) = get(&test_db, "/project/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Locked Team"), "{body}");
    assert!(!body.contains("Build Reviewers"), "{body}");
    // Milestones are never listed.
    let (status, body) = get(&test_db, "/project/query/all/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Build Reviewers"), "{body}");
    assert!(!body.contains("Sprint 1"), "{body}");
    // A key nobody saved.
    let (status, _) = get(&test_db, "/project/query/custom123/").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&test_db, "/project/query/all/?name=x").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);

    // Active people: Carol is disabled.
    let (status, body) = get(&test_db, "/people/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/p/alice/"), "{body}");
    assert!(!body.contains("/p/carol/"), "{body}");
    let (_, body) = get(&test_db, "/people/query/all/").await;
    assert!(body.contains("/p/carol/"), "{body}");
    let (status, _) = get(&test_db, "/people/query/approval/").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);

    let (status, body) = get(&test_db, "/people/manage/3/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("disabled this user."), "{body}");
    assert!(body.contains("Enable User"), "{body}");

    // Bob authored no dashboard; Alice's are active.
    let (status, body) = get(&test_db, "/dashboard/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("Team Board"), "{body}");
    let (_, body) = get(&test_db, "/dashboard/query/open/").await;
    assert!(body.contains("Team Board"), "{body}");
    assert!(!body.contains("Old Board"), "{body}");
    let (_, body) = get(&test_db, "/dashboard/panel/query/all/").await;
    assert!(body.contains("/W1"), "{body}");

    let (status, body) = get(&test_db, "/W1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<strong>team</strong>"), "{body}");
    let (status, _) = get(&test_db, "/W9").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = get(&test_db, "/favorites/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/project/edit/form/default/"), "{body}");
    assert!(!body.contains("/diffusion/edit/form/default/"), "{body}");
    assert!(body.contains("Edit Favorites"), "{body}");
}

#[tokio::test]
async fn members_are_added_and_removed_from_the_web() {
    let Some(test_db) = setup().await else {
        return;
    };
    let (status, body) = get(&test_db, "/project/members/1/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/project/members/1/add/"), "{body}");
    assert!(
        body.contains("/project/members/1/remove/?phid=PHID-USER-carol"),
        "{body}"
    );
    // Bob can't edit the locked project: no member actions.
    let (_, body) = get(&test_db, "/project/members/2/").await;
    assert!(!body.contains("/project/members/2/add/"), "{body}");
    assert!(body.contains("/project/silence/2/"), "{body}");

    let (status, body) = get(&test_db, "/project/members/1/add/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"name="rust.add""#), "{body}");
    // Milestones explain why they have no members of their own.
    let (status, body) = get(&test_db, "/project/members/3/add/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Unsupported Project"), "{body}");
    let (status, _) = get(&test_db, "/project/members/2/add/").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);

    // Names typed in Rust's input are checked, and nothing is written.
    let response = post(
        &test_db,
        "/project/members/1/add/",
        &[("rust.add", "alice")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains(r#"name="memberPHIDs[]" value="PHID-USER-alice" checked"#),
        "{body}"
    );
    assert_eq!(
        transactions(&test_db, "PHID-PROJ-build").await,
        Vec::<String>::new()
    );

    let response = post(
        &test_db,
        "/project/members/1/add/",
        &[("memberPHIDs[]", "PHID-USER-alice")],
    )
    .await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/members/1/"
    );
    assert_eq!(
        transactions(&test_db, "PHID-PROJ-build").await,
        vec![format!(
            r#"core:edge [] ["PHID-USER-alice"] {{"edge:type":13,"core.groupID":"G"}} PHID-USER-bob {WEB_SOURCE}"#
        )]
    );
    // Adding her again has no effect.
    let response = post(
        &test_db,
        "/project/members/1/add/",
        &[("memberPHIDs[]", "PHID-USER-alice")],
    )
    .await;
    assert_eq!(fallback_reason(&response), "edits without effect");
    let response = post(
        &test_db,
        "/project/members/1/add/",
        &[("memberPHIDs[]", "PHID-PROJ-locked")],
    )
    .await;
    assert_eq!(fallback_reason(&response), "members which are not users");

    let (status, body) = get(&test_db, "/project/members/1/remove/?phid=PHID-USER-carol").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("Remove carol as a project member of Build Reviewers?"),
        "{body}"
    );
    assert!(
        body.contains(r#"<input type="hidden" name="phid" value="PHID-USER-carol">"#),
        "{body}"
    );
    let response = post(
        &test_db,
        "/project/members/1/remove/",
        &[("phid", "PHID-USER-carol")],
    )
    .await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/members/1/"
    );
    assert_eq!(
        edges(&test_db, "project", 13).await,
        [
            "PHID-PROJ-build PHID-USER-alice",
            "PHID-PROJ-locked PHID-USER-bob",
        ]
    );
    // Milestones have no member list to edit.
    let (status, _) = get(&test_db, "/project/members/3/remove/?phid=PHID-USER-carol").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let response = post(
        &test_db,
        "/project/members/2/remove/",
        &[("phid", "PHID-USER-bob")],
    )
    .await;
    assert_eq!(fallback_reason(&response), "projects the viewer can't edit");

    // Watchers.
    let response = post(&test_db, "/project/watch/1/", &[("__dialog__", "1")]).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let (_, body) = get(&test_db, "/project/watchers/1/remove/?phid=PHID-USER-bob").await;
    assert!(
        body.contains("Remove bob as a watcher of Build Reviewers?"),
        "{body}"
    );
    let response = post(
        &test_db,
        "/project/watchers/1/remove/",
        &[("phid", "PHID-USER-bob")],
    )
    .await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/members/1/"
    );
    assert_eq!(edges(&test_db, "project", 47).await, Vec::<String>::new());
    assert_eq!(edges(&test_db, "user", 48).await, Vec::<String>::new());
}

#[tokio::test]
async fn members_silence_project_mail() {
    let Some(test_db) = setup().await else {
        return;
    };
    // Bob isn't a member of project 1.
    let (status, body) = get(&test_db, "/project/silence/1/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Not a Member"), "{body}");
    let (status, body) = get(&test_db, "/project/silence/2/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Disable Project Mail"), "{body}");
    let response = post(&test_db, "/project/silence/2/", &[]).await;
    assert_eq!(
        fallback_reason(&response),
        "posts which aren't dialog posts"
    );
    // Silencing needs no capability: Bob can't edit the locked project.
    let response = post(&test_db, "/project/silence/2/", &[("__dialog__", "1")]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/members/2/"
    );
    assert_eq!(
        transactions(&test_db, "PHID-PROJ-locked").await,
        vec![format!(
            r#"core:edge [] ["PHID-USER-bob"] {{"edge:type":61,"core.groupID":"G"}} PHID-USER-bob {WEB_SOURCE}"#
        )]
    );
    assert_eq!(
        edges(&test_db, "project", 61).await,
        ["PHID-PROJ-locked PHID-USER-bob"]
    );
    let (_, body) = get(&test_db, "/project/silence/2/").await;
    assert!(body.contains("Enable Project Mail"), "{body}");
    let response = post(&test_db, "/project/silence/2/", &[("__dialog__", "1")]).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(edges(&test_db, "project", 61).await, Vec::<String>::new());
    let (status, _) = get(&test_db, "/project/silence/3/").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn the_manage_page_shows_the_project_history() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = post(
        &test_db,
        "/project/edit/1/",
        &edit_form(&[("name", "Build Team"), ("color", "red")]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let response = post(&test_db, "/project/update/1/join/", &[]).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let (status, body) = get(&test_db, "/project/manage/1/").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains(r#"renamed this project from "Build Reviewers" to "Build Team"."#),
        "{body}"
    );
    assert!(body.contains("added 1 member(s): "), "{body}");
    assert!(
        body.contains("#build_reviewers, #builders, #build_team"),
        "{body}"
    );
    assert!(body.contains("/project/archive/1/"), "{body}");

    // The warning before the first subproject goes on to PHP's form.
    let (status, body) = get(&test_db, "/project/warning/1/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Convert Project"), "{body}");
    let response = post(&test_db, "/project/warning/1/", &[]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/project/edit/?parent=1"
    );
    let (status, _) = get(&test_db, "/project/warning/2/").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // `/project/filter/x/` is the default query.
    let (status, body) = get(&test_db, "/project/filter/x/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Locked Team"), "{body}");
}

#[tokio::test]
async fn lists_run_saved_queries() {
    let Some(test_db) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_search`.search_savedquery (engineClassName, parameters, \
               dateCreated, dateModified, queryKey) VALUES \
             ('PhabricatorProjectSearchEngine', '{\"name\":\"\",\"memberPHIDs\":[\"PHID-USER-carol\"],\"watcherPHIDs\":[],\"status\":\"active\",\"isMilestone\":false,\"isRoot\":null,\"icons\":[],\"colors\":[],\"query\":\"\",\"order\":\"name\"}', 1, 1, 'carolsprojs1'), \
             ('PhabricatorProjectSearchEngine', '{\"colors\":[\"red\"],\"order\":\"newest\"}', 1, 1, 'redprojects1'), \
             ('PhabricatorProjectSearchEngine', '{\"memberPHIDs\":[\"viewer()\"]}', 1, 1, 'viewerprojs1'), \
             ('PhabricatorProjectSearchEngine', '{\"query\":\"build\"}', 1, 1, 'fulltextprj1'), \
             ('PhabricatorPeopleSearchEngine', '{\"isDisabled\":true,\"usernames\":[],\"nameLike\":\"\"}', 1, 1, 'disabledppl1'), \
             ('PhabricatorPeopleSearchEngine', '{\"createdStart\":\"2020-01-01\"}', 1, 1, 'datedpeople1')",
        )
        .await;
    let (status, body) = get(&test_db, "/project/query/carolsprojs1/").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Advanced Search"), "{body}");
    assert!(body.contains("Build Reviewers"), "{body}");
    assert!(!body.contains("Locked Team"), "{body}");
    assert!(
        body.contains("/project/query/advanced/?query=carolsprojs1"),
        "{body}"
    );
    let (_, body) = get(&test_db, "/project/query/redprojects1/").await;
    assert!(body.contains("Locked Team"), "{body}");
    assert!(!body.contains("Build Reviewers"), "{body}");
    let (status, _) = get(&test_db, "/project/query/viewerprojs1/").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
    let (status, _) = get(&test_db, "/project/query/fulltextprj1/").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
    // A query of another engine.
    let (status, _) = get(&test_db, "/project/query/disabledppl1/").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);

    let (status, body) = get(&test_db, "/project/query/advanced/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"name="memberPHIDs""#), "{body}");
    assert!(
        body.contains(
            r#"<form class="settings-form" method="post" action="/project/query/advanced/">"#
        ),
        "{body}"
    );
    let (_, body) = get(&test_db, "/project/query/advanced/?query=carolsprojs1").await;
    assert!(
        body.contains(r#"name="memberPHIDs" value="carol""#),
        "{body}"
    );
    let (status, _) = get(&test_db, "/project/query/advanced/?query=nosuchquery1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = get(&test_db, "/people/query/disabledppl1/").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("/p/carol/"), "{body}");
    assert!(!body.contains("/p/alice/"), "{body}");
    let (status, _) = get(&test_db, "/people/query/datedpeople1/").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
    let (status, body) = get(&test_db, "/people/query/advanced/?query=disabledppl1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"<option value="true" selected>Show Only Disabled Users</option>"#),
        "{body}"
    );
}
