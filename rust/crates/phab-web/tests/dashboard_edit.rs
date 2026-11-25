//! The dashboard writes of the web interface against a test database: the
//! edit forms of panels and portals, archiving panels and dashboards, and
//! the read pages around them (the console, the portal list, portal items
//! and the "configure" routes of the menus).

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
use serde_json::{Map, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
/// A user's `accountSecret`, which fills its `binary(64)` column.
const ACCOUNT_SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
const BOB: &str = "phsid=bob-token; phab_ui=rust";
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;

const DATABASES: &[&str] = &[
    "user",
    "policy",
    "spaces",
    "conduit",
    "auth",
    "herald",
    "worker",
    "search",
    "dashboard",
    "project",
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

/// Bob has a session. Panel 1 is a text panel, panel 2 a query panel,
/// panel 3 one Bob can't edit, panel 4 an archived tab panel. Portal 1
/// has a dashboard item, a link and a label; Bob can't edit portal 2.
/// Dashboard 1 is active, dashboard 2 archived.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (1, 'PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, '{secret}', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial, \
                highSecurityUntil) VALUES \
             ('PHID-USER-bob', 'web', '{bob}', 1699000000, {expires}, 'PHID-SSSN-1', 0, NULL); \
             INSERT INTO `{{ns}}_dashboard`.dashboard_panel (id, phid, name, viewPolicy, \
               editPolicy, panelType, authorPHID, isArchived, properties, dateCreated, \
               dateModified) VALUES \
             (1, 'PHID-DSHP-1', 'Welcome', 'users', 'users', 'text', 'PHID-USER-bob', 0, \
               '{{\"text\":\"Hello\"}}', 5, 5), \
             (2, 'PHID-DSHP-2', 'Queries', 'users', 'users', 'query', 'PHID-USER-bob', 0, \
               '{{\"class\":\"x\",\"key\":\"all\"}}', 5, 5), \
             (3, 'PHID-DSHP-3', 'Locked', 'users', 'admin', 'text', 'PHID-USER-bob', 0, \
               '{{\"text\":\"\"}}', 5, 5), \
             (4, 'PHID-DSHP-4', 'Tabs', 'users', 'users', 'tabs', 'PHID-USER-bob', 1, \
               '[]', 5, 5); \
             INSERT INTO `{{ns}}_dashboard`.dashboard_portal (id, phid, name, status, \
               viewPolicy, editPolicy, properties, dateCreated, dateModified) VALUES \
             (1, 'PHID-PRTL-1', 'Home', 'active', 'users', 'users', '[]', 5, 5), \
             (2, 'PHID-PRTL-2', 'Admins', 'active', 'users', 'admin', '[]', 5, 5); \
             INSERT INTO `{{ns}}_dashboard`.dashboard (id, phid, name, layoutConfig, viewPolicy, \
               editPolicy, dateCreated, dateModified, status, authorPHID, icon) VALUES \
             (1, 'PHID-DSHB-1', 'Team', '[]', 'users', 'users', 5, 5, 'active', \
               'PHID-USER-bob', 'fa-dashboard'), \
             (2, 'PHID-DSHB-2', 'Old', '{{\"layoutMode\":\"layout-mode-full\"}}', 'users', \
               'users', 5, 5, 'archived', 'PHID-USER-bob', 'fa-dashboard'); \
             INSERT INTO `{{ns}}_search`.search_profilepanelconfiguration (id, phid, \
               profilePHID, menuItemKey, builtinKey, menuItemOrder, visibility, \
               menuItemProperties, dateCreated, dateModified, customPHID) VALUES \
             (1, 'PHID-PANL-1', 'PHID-PRTL-1', 'dashboard', NULL, 1, 'visible', \
               '{{\"dashboardPHID\":\"PHID-DSHB-1\"}}', 1, 1, NULL), \
             (2, 'PHID-PANL-2', 'PHID-PRTL-1', 'dashboard', NULL, 2, 'visible', \
               '{{\"dashboardPHID\":\"PHID-DSHB-2\"}}', 1, 1, NULL), \
             (3, 'PHID-PANL-3', 'PHID-PRTL-1', 'link', NULL, 3, 'visible', \
               '{{\"name\":\"Docs\",\"uri\":\"/docs/\"}}', 1, 1, NULL)",
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

/// Gets a page as Bob.
async fn get(test_db: &TestDatabase, path: &str) -> Response {
    let request = Request::get(path)
        .header(header::COOKIE, BOB)
        .header(header::HOST, "phabricator.test")
        .body(Body::empty())
        .unwrap();
    send(test_db, request).await
}

/// Gets a page as Bob: its status and body.
async fn page(test_db: &TestDatabase, path: &str) -> (StatusCode, String) {
    let response = get(test_db, path).await;
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

/// The transactions of `table`: object, type, values, author, content
/// source.
async fn transactions(test_db: &TestDatabase, table: &str) -> Vec<String> {
    lines(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' ', objectPHID, transactionType, oldValue, newValue, \
               authorPHID, contentSource) AS line \
             FROM `{{ns}}_dashboard`.{table} ORDER BY id"
        ),
    )
    .await
}

fn panel_form<'a>(changes: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("editEngine", "true"),
        ("name", "Welcome"),
        ("text", "Hello"),
        ("text_metadata", "{}"),
        ("policy.view", "users"),
        ("policy.edit", "users"),
    ];
    for (key, value) in changes {
        fields
            .iter_mut()
            .find(|(known, _)| known == key)
            .expect("a field of the form")
            .1 = value;
    }
    fields
}

#[tokio::test]
async fn panels_and_portals_are_edited_from_the_web() {
    let Some(test_db) = setup().await else {
        return;
    };
    let (status, body) = page(&test_db, "/dashboard/panel/edit/1/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Edit Panel: Welcome"), "{body}");
    assert!(body.contains(">Hello</textarea>"), "{body}");
    assert_eq!(
        fallback_reason(&get(&test_db, "/dashboard/panel/edit/2/").await),
        "the form of query panels"
    );
    assert_eq!(
        fallback_reason(&get(&test_db, "/dashboard/panel/edit/3/").await),
        "objects the viewer can't edit"
    );
    assert_eq!(
        get(&test_db, "/dashboard/panel/edit/99/").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fallback_reason(&get(&test_db, "/dashboard/panel/edit/1/?contextPHID=PHID-DSHB-1").await),
        "edit forms with parameters"
    );

    // Left to PHP: other changes, no change, missing fields, bad names.
    for (changes, reason) in [
        (vec![("text", "Bye")], "text changes"),
        (vec![("policy.edit", "admin")], "policy changes"),
        (vec![], "edits without effect"),
        (vec![("name", "")], "edits PHP refuses"),
    ] {
        let response = post(&test_db, "/dashboard/panel/edit/1/", &panel_form(&changes)).await;
        assert_eq!(fallback_reason(&response), reason, "{changes:?}");
    }
    let response = post(
        &test_db,
        "/dashboard/panel/edit/1/",
        &[("editEngine", "true"), ("name", "X")],
    )
    .await;
    assert_eq!(fallback_reason(&response), "form posts without every field");
    assert_eq!(
        transactions(&test_db, "dashboard_paneltransaction").await,
        Vec::<String>::new()
    );

    let response = post(
        &test_db,
        "/dashboard/panel/edit/1/",
        &panel_form(&[("name", "Hi there")]),
    )
    .await;
    assert_eq!(location(&response), "http://phabricator.test/W1");
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        cookie.starts_with("phfrm=edit-engine%2Fdashboard.panel")
            || cookie.starts_with("phfrm=edit-engine/dashboard.panel"),
        "{cookie}"
    );
    assert_eq!(
        transactions(&test_db, "dashboard_paneltransaction").await,
        [format!(
            "PHID-DSHP-1 dashpanel:name \"Welcome\" \"Hi there\" PHID-USER-bob {WEB_SOURCE}"
        )]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', name, dateModified) AS line \
             FROM `{ns}_dashboard`.dashboard_panel WHERE id = 1",
        )
        .await,
        [format!("Hi there {NOW}")]
    );

    // Portals: the tags must be posted unchanged.
    let (status, body) = page(&test_db, "/portal/edit/1/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Edit Portal: Home"), "{body}");
    let portal = [
        ("editEngine", "true"),
        ("name", "Start"),
        ("policy.view", "users"),
        ("policy.edit", "users"),
    ];
    let mut tagged = portal.to_vec();
    tagged.push(("projectPHIDs[]", "PHID-PROJ-x"));
    assert_eq!(
        fallback_reason(&post(&test_db, "/portal/edit/1/", &tagged).await),
        "tag changes"
    );
    let response = post(&test_db, "/portal/edit/1/", &portal).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/portal/view/1/view/manage/"
    );
    assert_eq!(
        transactions(&test_db, "dashboard_portaltransaction").await,
        [format!(
            "PHID-PRTL-1 name \"Home\" \"Start\" PHID-USER-bob {WEB_SOURCE}"
        )]
    );
    assert_eq!(
        fallback_reason(&post(&test_db, "/portal/edit/2/", &portal).await),
        "objects the viewer can't edit"
    );
    let tasks = lines(
        &test_db,
        "SELECT objectPHID AS line FROM `{ns}_worker`.worker_activetask ORDER BY id",
    )
    .await;
    assert_eq!(tasks, ["PHID-DSHP-1", "PHID-PRTL-1"]);

    test_db.drop_databases().await;
}

#[tokio::test]
async fn panels_and_dashboards_are_archived_from_the_web() {
    let Some(test_db) = setup().await else {
        return;
    };
    let (status, body) = page(&test_db, "/dashboard/panel/archive/1/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Archive Panel?"), "{body}");
    let (_, body) = page(&test_db, "/dashboard/panel/archive/4/").await;
    assert!(body.contains("Activate Panel?"), "{body}");
    // PHP's query requires CAN_EDIT: Bob sees PHP's policy dialog.
    assert_eq!(
        fallback_reason(&get(&test_db, "/dashboard/panel/archive/3/").await),
        "objects the viewer can't edit"
    );

    let response = post(&test_db, "/dashboard/panel/archive/1/", &[]).await;
    assert_eq!(location(&response), "http://phabricator.test/W1");
    let response = post(&test_db, "/dashboard/panel/archive/4/", &[]).await;
    assert_eq!(location(&response), "http://phabricator.test/W4");
    assert_eq!(
        transactions(&test_db, "dashboard_paneltransaction").await,
        [
            format!("PHID-DSHP-1 dashpanel:archive false true PHID-USER-bob {WEB_SOURCE}"),
            format!("PHID-DSHP-4 dashpanel:archive true false PHID-USER-bob {WEB_SOURCE}"),
        ]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', id, isArchived, properties) AS line \
             FROM `{ns}_dashboard`.dashboard_panel WHERE id IN (1, 4) ORDER BY id",
        )
        .await,
        ["1 1 {\"text\":\"Hello\"}", "4 0 []"]
    );

    let (_, body) = page(&test_db, "/dashboard/archive/1/").await;
    assert!(
        body.contains("This dashboard will be marked as archived."),
        "{body}"
    );
    let response = post(&test_db, "/dashboard/archive/1/", &[]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/dashboard/view/1/"
    );
    let response = post(&test_db, "/dashboard/archive/2/", &[]).await;
    assert_eq!(
        location(&response),
        "http://phabricator.test/dashboard/view/2/"
    );
    assert_eq!(
        transactions(&test_db, "dashboard_transaction").await,
        [
            format!(
                "PHID-DSHB-1 dashboard:status \"active\" \"archived\" PHID-USER-bob {WEB_SOURCE}"
            ),
            format!(
                "PHID-DSHB-2 dashboard:status \"archived\" \"active\" PHID-USER-bob {WEB_SOURCE}"
            ),
        ]
    );
    assert_eq!(
        lines(
            &test_db,
            "SELECT CONCAT_WS(' ', status, layoutConfig) AS line \
             FROM `{ns}_dashboard`.dashboard ORDER BY id",
        )
        .await,
        [
            "archived []",
            "active {\"layoutMode\":\"layout-mode-full\"}"
        ]
    );

    // Posts which aren't form posts go to PHP.
    let request = Request::post("/dashboard/archive/1/")
        .header(header::COOKIE, BOB)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("x=1"))
        .unwrap();
    assert_eq!(
        fallback_reason(&send(&test_db, request).await),
        "requests which aren't form posts"
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn console_portals_and_menus_read_like_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let (status, body) = page(&test_db, "/dashboard/console/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Dashboard Console"), "{body}");
    assert!(body.contains("<a href=\"/portal/\">Portals</a>"), "{body}");

    let (status, body) = page(&test_db, "/portal/").await;
    assert_eq!(status, StatusCode::OK);
    let admins = body.find("Admins").expect("portal 2");
    let home = body.find(">Home</a>").expect("portal 1");
    assert!(admins < home, "{body}");
    assert!(body.contains("Portal 1"), "{body}");
    assert_eq!(page(&test_db, "/portal/query/all/").await.0, StatusCode::OK);
    assert_eq!(
        fallback_reason(&get(&test_db, "/portal/query/mine/").await),
        "a saved search query"
    );

    // Portal items: a dashboard, an archived one, a link, nothing.
    let (status, body) = page(&test_db, "/portal/view/1/view/1/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<title>Team · Phabricator</title>"), "{body}");
    let (_, body) = page(&test_db, "/portal/view/1/view/2/").await;
    assert!(body.contains("This dashboard has been archived."), "{body}");
    assert_eq!(
        fallback_reason(&get(&test_db, "/portal/view/1/view/3/").await),
        "portal items other than dashboards"
    );
    let (_, body) = page(&test_db, "/portal/view/1/view/99/").await;
    assert!(
        body.contains("No visible menu items can render content."),
        "{body}"
    );

    // Configure: Bob edits portal 1, not portal 2, nor the applications.
    assert_eq!(
        location(&get(&test_db, "/portal/view/1/configure/").await),
        "http://phabricator.test/portal/view/1/configure/global/"
    );
    assert_eq!(
        get(&test_db, "/portal/view/2/configure/").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        location(&get(&test_db, "/home/menu/configure/").await),
        "http://phabricator.test/home/menu/configure/custom/"
    );
    assert_eq!(
        location(&get(&test_db, "/favorites/menu/configure/").await),
        "http://phabricator.test/favorites/menu/configure/custom/"
    );

    test_db.drop_databases().await;
}
