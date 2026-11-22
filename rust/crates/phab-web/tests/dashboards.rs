//! The dashboard and portal pages against one test database.
//!
//! Bob sees dashboard 1 but not every panel on it, and can't edit it;
//! Alice can. Portal 1's menu has dashboards Bob can and can't see.

// The checks share one database, so each page's are one long function.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::{FALLBACK_HEADER, WebState};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

const ALICE: &str = "phsid=alice-token";
const BOB: &str = "phsid=bob-token";
const ROOT: &str = "phsid=root-token";

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
        "dashboard",
        "search",
        "project",
        "policy",
        "spaces",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-root', 'root', 'Root', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0), \
             ('PHID-USER-bob', 'web', '{bob}', 1, {expires}, 'PHID-SSSN-2', 0), \
             ('PHID-USER-root', 'web', '{root}', 1, {expires}, 'PHID-SSSN-3', 0)",
            alice = session_key("alice-token"),
            bob = session_key("bob-token"),
            root = session_key("root-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    test_db.execute(DASHBOARDS).await;
    test_db.execute(PORTALS).await;
    Some(test_db)
}

/// Dashboard 1 has two columns: a text panel and one only Alice sees on
/// the left; a tab panel and a deleted panel on the right. Alice edits
/// it. Nobody sees dashboard 2; dashboard 3 has a query panel; dashboard 4
/// is archived.
const DASHBOARDS: &str = "\
    INSERT INTO `{ns}_dashboard`.dashboard (id, phid, name, layoutConfig, viewPolicy, \
      editPolicy, dateCreated, dateModified, status, authorPHID, icon) VALUES \
    (1, 'PHID-DSHB-1', 'Team', '{\"layoutMode\":\"layout-mode-half-and-half\",\"panels\":[\
      {\"panelKey\":\"k1\",\"panelPHID\":\"PHID-DSHP-1\",\"columnKey\":\"left\"},\
      {\"panelKey\":\"k2\",\"panelPHID\":\"PHID-DSHP-2\",\"columnKey\":\"left\"},\
      {\"panelKey\":\"k3\",\"panelPHID\":\"PHID-DSHP-3\",\"columnKey\":\"right\"},\
      {\"panelKey\":\"k4\",\"panelPHID\":\"PHID-DSHP-gone\",\"columnKey\":\"right\"}]}', \
      'users', 'PHID-USER-alice', 1, 1, 'active', 'PHID-USER-alice', 'fa-users'), \
    (2, 'PHID-DSHB-2', 'Private', '{}', 'no-one', 'no-one', 1, 1, 'active', \
      'PHID-USER-alice', 'fa-dashboard'), \
    (3, 'PHID-DSHB-3', 'Queries', '{\"panels\":[{\"panelKey\":\"q\",\"panelPHID\":\"PHID-DSHP-4\"}]}', \
      'users', 'users', 1, 1, 'active', 'PHID-USER-alice', 'fa-dashboard'), \
    (4, 'PHID-DSHB-4', 'Old', '[]', 'users', 'users', 1, 1, 'archived', 'PHID-USER-alice', \
      'fa-dashboard'); \
    INSERT INTO `{ns}_dashboard`.dashboard_panel (id, phid, name, panelType, viewPolicy, \
      editPolicy, isArchived, properties, dateCreated, dateModified, authorPHID) VALUES \
    (1, 'PHID-DSHP-1', 'Hello', 'text', 'users', 'users', 0, \
      '{\"text\":\"Hello **world**\"}', 1, 1, 'PHID-USER-alice'), \
    (2, 'PHID-DSHP-2', 'Secret', 'text', 'PHID-USER-alice', 'users', 0, \
      '{\"text\":\"Top secret\"}', 1, 1, 'PHID-USER-alice'), \
    (3, 'PHID-DSHP-3', 'Tabs', 'tabs', 'users', 'users', 0, \
      '{\"config\":[{\"panelID\":1,\"name\":\"\"},{\"panelID\":\"2\",\"name\":\"Hush\"},\
      {\"panelID\":99}]}', 1, 1, 'PHID-USER-alice'), \
    (4, 'PHID-DSHP-4', 'Revisions', 'query', 'users', 'users', 0, \
      '{\"class\":\"DifferentialRevisionSearchEngine\",\"key\":\"all\"}', 1, 1, \
      'PHID-USER-alice'); \
    INSERT INTO `{ns}_dashboard`.dashboard_transaction (phid, authorPHID, objectPHID, \
      viewPolicy, editPolicy, commentPHID, commentVersion, transactionType, oldValue, newValue, \
      contentSource, metadata, dateCreated, dateModified) VALUES \
    ('PHID-XACT-DSHB-1', 'PHID-USER-alice', 'PHID-DSHB-1', 'users', 'users', NULL, 0, \
      'core:create', 'null', 'null', '{}', '{\"core.create\":true}', 10, 10), \
    ('PHID-XACT-DSHB-2', 'PHID-USER-alice', 'PHID-DSHB-1', 'users', 'users', NULL, 0, \
      'dashboard:name', '\"\"', '\"Old\"', '{}', '{\"core.create\":true}', 10, 10), \
    ('PHID-XACT-DSHB-3', 'PHID-USER-alice', 'PHID-DSHB-1', 'users', 'users', NULL, 0, \
      'dashboard:name', '\"Old\"', '\"Team\"', '{}', '{}', 20, 20), \
    ('PHID-XACT-DSHB-4', 'PHID-USER-alice', 'PHID-DSHB-1', 'users', 'users', NULL, 0, \
      'panels', '[]', '[]', '{}', '{}', 30, 30)";

/// Portal 1's menu: a label, Dashboard 1, the dashboard nobody sees, a
/// link and the archived dashboard (pinned, but it can't be shown).
/// Nobody sees portal 2; portal 3 has an application item; portal 4 has
/// nothing to show.
const PORTALS: &str = "\
    INSERT INTO `{ns}_dashboard`.dashboard_portal (id, phid, name, status, viewPolicy, \
      editPolicy, properties, dateCreated, dateModified) VALUES \
    (1, 'PHID-PRTL-1', 'Home', 'active', 'users', 'admin', '{}', 1, 1), \
    (2, 'PHID-PRTL-2', 'Hidden', 'active', 'no-one', 'no-one', '{}', 1, 1), \
    (3, 'PHID-PRTL-3', 'Apps', 'active', 'users', 'users', '{}', 1, 1), \
    (4, 'PHID-PRTL-4', 'Empty', 'active', 'users', 'users', '{}', 1, 1); \
    INSERT INTO `{ns}_search`.search_profilepanelconfiguration (id, phid, profilePHID, \
      menuItemKey, builtinKey, menuItemOrder, visibility, menuItemProperties, dateCreated, \
      dateModified, customPHID) VALUES \
    (1, 'PHID-PANL-1', 'PHID-PRTL-1', 'label', NULL, 1, 'visible', '{\"name\":\"Start\"}', 1, 1, \
      NULL), \
    (2, 'PHID-PANL-2', 'PHID-PRTL-1', 'dashboard', NULL, 2, 'visible', \
      '{\"dashboardPHID\":\"PHID-DSHB-1\",\"name\":\"\"}', 1, 1, NULL), \
    (3, 'PHID-PANL-3', 'PHID-PRTL-1', 'dashboard', NULL, 3, 'visible', \
      '{\"dashboardPHID\":\"PHID-DSHB-2\"}', 1, 1, NULL), \
    (4, 'PHID-PANL-4', 'PHID-PRTL-1', 'link', NULL, 4, 'visible', \
      '{\"name\":\"Docs\",\"uri\":\"https://docs.example/\",\"icon\":\"feed\",\
      \"tooltip\":\"Read\"}', 1, 1, NULL), \
    (5, 'PHID-PANL-5', 'PHID-PRTL-1', 'dashboard', NULL, 5, 'default', \
      '{\"dashboardPHID\":\"PHID-DSHB-4\"}', 1, 1, NULL), \
    (6, 'PHID-PANL-6', 'PHID-PRTL-1', 'link', NULL, 6, 'disabled', \
      '{\"name\":\"Gone\",\"uri\":\"https://gone.example/\"}', 1, 1, NULL), \
    (7, 'PHID-PANL-7', 'PHID-PRTL-3', 'application', NULL, 1, 'visible', \
      '{\"application\":\"PHID-APPS-x\"}', 1, 1, NULL), \
    (8, 'PHID-PANL-8', 'PHID-PRTL-2', 'dashboard', NULL, 1, 'visible', \
      '{\"dashboardPHID\":\"PHID-DSHB-1\"}', 1, 1, NULL), \
    (9, 'PHID-PANL-9', 'PHID-PRTL-1', 'menu.manage', 'item.configure', 9, 'visible', \
      '{\"name\":\"Change Menu\"}', 1, 1, NULL); \
    INSERT INTO `{ns}_search`.edge (src, type, dst, dateCreated, seq) VALUES \
    ('PHID-PANL-2', 70, 'PHID-DSHB-1', 1, 0), ('PHID-PANL-8', 70, 'PHID-DSHB-1', 1, 0)";

async fn get(test_db: &TestDatabase, path: &str, cookie: Option<&str>) -> Response {
    let config = Config::default();
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn page(test_db: &TestDatabase, path: &str, cookie: &str) -> String {
    let response = get(test_db, path, Some(cookie)).await;
    let status = response.status();
    let reason = response
        .headers()
        .get(FALLBACK_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{path} as {cookie}: {reason:?} {text}"
    );
    text
}

async fn status(test_db: &TestDatabase, path: &str, cookie: &str) -> StatusCode {
    get(test_db, path, Some(cookie)).await.status()
}

async fn fallback(test_db: &TestDatabase, path: &str, cookie: Option<&str>) -> String {
    let response = get(test_db, path, cookie).await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    response.headers()[FALLBACK_HEADER]
        .to_str()
        .unwrap()
        .to_owned()
}

async fn dashboards(test_db: &TestDatabase) {
    // Bob sees the panels he may see, and can't edit.
    let text = page(test_db, "/dashboard/view/1/", BOB).await;
    assert!(
        text.contains("<span class=\"fa fa-users\" aria-hidden=\"true\"></span> Team</h1>"),
        "{text}"
    );
    assert!(text.contains("Hello <strong>world</strong>"), "{text}");
    assert!(text.contains("<h2>Hello</h2>"), "{text}");
    assert!(text.contains("<h2>Restricted Panel</h2>"), "{text}");
    assert!(
        text.contains("You do not have permission to see this panel."),
        "{text}"
    );
    assert!(text.contains("<h2>Invalid Panel</h2>"), "{text}");
    assert!(!text.contains("Top secret"), "{text}");
    // The tabs: the first named after its panel, Bob can't see the second,
    // the third doesn't exist.
    assert!(
        text.contains("<li>Hello</li><li>Hush</li><li>Unnamed Tab</li>"),
        "{text}"
    );
    assert_eq!(text.matches("(Invalid Panel)").count(), 2, "{text}");
    assert!(text.contains("dashboard-column dashboard-half"), "{text}");
    assert!(!text.contains("Edit Panel"), "{text}");
    assert!(
        text.contains("<span class=\"hm-disabled\" aria-disabled=\"true\"><span class=\"fa fa-ban\" aria-hidden=\"true\"></span> Archive Dashboard</span>"),
        "{text}"
    );
    assert!(
        text.contains("<a href=\"/dashboard/install/1/\">"),
        "{text}"
    );
    // Used by portal 1, not by the portal Bob can't see.
    assert!(text.contains("<td>Portal Menu</td><td class=\"hm-pri\"><a href=\"/portal/view/1/\">Home</a></td><td>Global</td>"), "{text}");
    assert!(!text.contains("Hidden"), "{text}");
    assert!(
        text.contains("<a href=\"/p/alice/\">alice</a> created this object."),
        "{text}"
    );
    assert!(
        text.contains("renamed this dashboard from \"Old\" to \"Team\"."),
        "{text}"
    );
    assert!(!text.contains("from \"\""), "{text}");
    assert!(
        text.contains("changed the panels on this dashboard."),
        "{text}"
    );

    // Alice edits it: monograms and the panel commands.
    let text = page(test_db, "/dashboard/view/1/", ALICE).await;
    assert!(text.contains("<h2>W1 Hello "), "{text}");
    assert!(text.contains("Top secret"), "{text}");
    assert!(
        text.contains("href=\"/dashboard/panel/edit/1/?contextPHID=PHID-DSHB-1\""),
        "{text}"
    );
    assert!(
        text.contains("href=\"/dashboard/adjust/remove/?contextPHID=PHID-DSHB-1&amp;panelKey=k4\""),
        "{text}"
    );
    assert!(text.contains("Create Panel"), "{text}");
    assert!(text.contains("<a href=\"/dashboard/archive/1/\"><span class=\"fa fa-ban\" aria-hidden=\"true\"></span> Archive Dashboard</a>"), "{text}");

    let text = page(test_db, "/dashboard/view/4/", BOB).await;
    assert!(text.contains("Archived"), "{text}");
    assert!(text.contains("Activate Dashboard"), "{text}");
    assert!(
        text.contains("This dashboard has not been added to any menus."),
        "{text}"
    );

    assert_eq!(
        status(test_db, "/dashboard/view/2/", BOB).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(test_db, "/dashboard/view/99/", BOB).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fallback(test_db, "/dashboard/view/3/", Some(BOB)).await,
        "query panels"
    );
    assert_eq!(
        fallback(test_db, "/dashboard/view/1/", None).await,
        "logged out"
    );
}

async fn portals(test_db: &TestDatabase) {
    let text = page(test_db, "/portal/view/1/", BOB).await;
    assert!(
        text.contains("<span class=\"fa fa-compass\" aria-hidden=\"true\"></span> Home</h1>"),
        "{text}"
    );
    assert!(
        text.contains("<li class=\"portal-label\">Start</li>"),
        "{text}"
    );
    // The first dashboard which can be shown: the pinned one is archived.
    assert!(
        text.contains("<li class=\"portal-selected\"><a href=\"/portal/view/1/view/2/\" aria-current=\"page\"><span class=\"fa fa-users\" aria-hidden=\"true\"></span> Team</a></li>"),
        "{text}"
    );
    assert!(text.contains("<h2>Team</h2>"), "{text}");
    assert!(text.contains("<title>Team · Phabricator</title>"), "{text}");
    assert!(text.contains("Hello <strong>world</strong>"), "{text}");
    // Not in edit mode, even for viewers who can edit the dashboard.
    assert!(!text.contains("Edit Panel"), "{text}");
    assert!(
        text.contains(
            "<span class=\"fa fa-ban\" aria-hidden=\"true\"></span> Restricted Dashboard</span>"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "<span class=\"fa fa-ban\" aria-hidden=\"true\"></span> Archived Dashboard</span>"
        ),
        "{text}"
    );
    assert!(text.contains("<a href=\"https://docs.example/\" title=\"Read\"><span class=\"fa fa-newspaper-o\" aria-hidden=\"true\"></span> Docs</a>"), "{text}");
    assert!(!text.contains("Gone"), "{text}");
    assert!(text.contains("<a href=\"/portal/view/1/configure/\"><span class=\"fa fa-pencil\" aria-hidden=\"true\"></span> Change Menu</a>"), "{text}");
    assert!(text.contains("<a href=\"/portal/view/1/view/manage/\"><span class=\"fa fa-pencil\" aria-hidden=\"true\"></span> Manage Portal</a>"), "{text}");
    // The builtin items go last, by order: the stored "Edit Menu" moved
    // after "Manage Portal".
    let label = text.find("portal-label").unwrap();
    let divider = text.find("portal-divider").unwrap();
    let manage = text.find("Change Menu").unwrap();
    let portal = text.find("Manage Portal").unwrap();
    assert!(
        label < divider && divider < portal && portal < manage,
        "{text}"
    );

    let text = page(test_db, "/portal/view/4/", BOB).await;
    assert!(
        text.contains("<title>Empty · Phabricator</title>"),
        "{text}"
    );
    assert!(
        text.contains("No visible menu items can render content."),
        "{text}"
    );

    assert_eq!(
        status(test_db, "/portal/view/2/", BOB).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(test_db, "/portal/view/99/", BOB).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fallback(test_db, "/portal/view/3/", Some(BOB)).await,
        "a portal menu item not ported"
    );
}

#[tokio::test]
async fn dashboard_pages_show_what_the_viewer_may_see() {
    let Some(test_db) = setup().await else {
        return;
    };
    dashboards(&test_db).await;
    portals(&test_db).await;
    let _ = ROOT;
    test_db.drop_databases().await;
}
