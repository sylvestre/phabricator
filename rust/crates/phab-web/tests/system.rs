//! The read-only pages of Applications, Config, Daemons, Conduit, Policy,
//! Spaces, `MetaMTA`, Help, Search and Home, against a test database.

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
const ALICE: &str = "phsid=alice-token; phab_ui=rust";
const ROOT: &str = "phsid=root-token; phab_ui=rust";

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

/// `alice`, and `root`, an administrator, with their sessions.
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
        "file",
        "harbormaster",
        "search",
        "config",
        "worker",
        "daemon",
        "metamta",
        "application",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-root', 'root', 'Root', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0), \
             ('PHID-USER-root', 'web', '{root}', 1, {expires}, 'PHID-SSSN-2', 0)",
            alice = session_key("alice-token"),
            root = session_key("root-token"),
            expires = NOW + 29 * 86_400,
        ))
        .await;
    Some(test_db)
}

async fn send(
    test_db: &TestDatabase,
    config: Config,
    request: axum::http::request::Builder,
    cookie: &str,
) -> Response {
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(
            request
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn get(test_db: &TestDatabase, path: &str, cookie: &str) -> Response {
    send(test_db, Config::default(), Request::get(path), cookie).await
}

async fn text(response: Response) -> String {
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The page at `path`, which must answer 200.
async fn page(test_db: &TestDatabase, path: &str, cookie: &str) -> String {
    let response = get(test_db, path, cookie).await;
    let status = response.status();
    let body = text(response).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    body
}

fn fallback_reason(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    response.headers()[FALLBACK_HEADER].to_str().unwrap()
}

async fn applications_list_and_detail(test_db: &TestDatabase) {
    let launcher = page(test_db, "/applications/", ALICE).await;
    assert!(
        launcher.contains("<h2>Core Applications</h2>"),
        "{launcher}"
    );
    assert!(launcher.contains("<a href=\"/differential/\">Differential</a>"));
    assert!(launcher.contains("href=\"/applications/view/PhabricatorDifferentialApplication/\""));
    // Applications which aren't launchable are only in "All".
    assert!(!launcher.contains(">Policy</"));
    let all = page(test_db, "/applications/query/all/", ALICE).await;
    assert!(all.contains("<strong>Policy</strong>"), "{all}");
    // Moz-extensions applications are extensions.
    assert!(all.contains("<span class=\"tag\">Extension</span>"));

    let detail = page(
        test_db,
        "/applications/view/PhabricatorDifferentialApplication/",
        ALICE,
    )
    .await;
    assert!(
        detail.contains("<li class=\"tag\">Installed</li>"),
        "{detail}"
    );
    assert!(detail.contains("<dt>Can Use Application</dt>"));
    assert!(detail.contains(
        "<a href=\"/policy/explain/PHID-APPS-PhabricatorDifferentialApplication/edit/\">\
         <span class=\"fa fa-eye\" aria-hidden=\"true\"></span> Administrators</a>"
    ));
    assert!(detail.contains("<dt>Default View Policy</dt>"));
    // Alice can't configure it.
    assert!(detail.contains("<span class=\"hm-disabled\" aria-disabled=\"true\"><span class=\"fa fa-pencil\" aria-hidden=\"true\"></span> Edit Policies</span>"));

    let response = get(test_db, "/applications/view/NoSuchApplication/", ALICE).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = get(
        test_db,
        "/applications/view/PhabricatorFilesApplication/",
        ALICE,
    )
    .await;
    assert_eq!(
        fallback_reason(&response),
        "an application configuration panel"
    );
    let response = get(test_db, "/applications/query/abcdef/", ALICE).await;
    assert_eq!(fallback_reason(&response), "a saved application query");
}

async fn policies_of_applications_are_explained(test_db: &TestDatabase) {
    let explain = page(
        test_db,
        "/policy/explain/PHID-APPS-PhabricatorDifferentialApplication/view/",
        ALICE,
    )
    .await;
    assert!(
        explain.contains("Policy Details: Differential"),
        "{explain}"
    );
    assert!(explain.contains("<strong>Can View:</strong> All Users"));
    assert!(explain.contains("<li>Logged in users can take this action.</li>"));
    let response = get(
        test_db,
        "/policy/explain/PHID-APPS-PhabricatorDifferentialApplication/nope/",
        ALICE,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = get(test_db, "/policy/explain/PHID-DREV-x/view/", ALICE).await;
    assert_eq!(
        fallback_reason(&response),
        "the policies of an object not ported"
    );
}

async fn configuration_is_for_administrators(test_db: &TestDatabase) {
    for path in [
        "/config/",
        "/config/settings/",
        "/daemon/",
        "/daemon/task/1/",
    ] {
        let response = get(test_db, path, ALICE).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    test_db
        .execute(
            "INSERT INTO `{ns}_config`.config_entry (phid, namespace, configKey, value, \
               isDeleted, dateCreated, dateModified) VALUES \
             ('PHID-CONF-1', 'default', 'auth.email-domains', '[\"example.com\"]', 0, 1, 1)",
        )
        .await;
    let console = page(test_db, "/config/", ROOT).await;
    assert!(
        console.contains("<a href=\"/config/settings/\">Settings</a>"),
        "{console}"
    );

    let core = page(test_db, "/config/settings/", ROOT).await;
    assert!(core.contains(">auth.email-domains</a>"), "{core}");
    assert!(core.contains("<span class=\"tag\">Editable</span>"));
    assert!(!core.contains(">phabricator.base-uri</a>"));
    let advanced = page(test_db, "/config/settings/advanced/", ROOT).await;
    assert!(advanced.contains(">phabricator.base-uri</a>"));
    assert!(!advanced.contains(">auth.email-domains</a>"));

    let option = page(test_db, "/config/edit/auth.email-domains/", ROOT).await;
    assert!(
        option.contains("<h2>Current Configuration</h2>"),
        "{option}"
    );
    assert!(option.contains("<td>Database</td><td><pre>example.com</pre></td>"));
    assert!(option.contains("<td>Local Config</td><td><em>(No Value Configured)</em></td>"));
    assert!(option.contains("<td>Global Default</td><td><pre></pre></td>"));
    let locked = page(test_db, "/config/edit/phabricator.base-uri/", ROOT).await;
    assert!(locked.contains("<li class=\"tag\">Locked</li>"));

    let response = send(
        test_db,
        Config::default(),
        Request::post("/config/edit/auth.email-domains/"),
        ROOT,
    )
    .await;
    assert_eq!(fallback_reason(&response), "a form");
    let response = get(test_db, "/config/edit/no.such-option/", ROOT).await;
    assert_eq!(
        fallback_reason(&response),
        "an unknown configuration option"
    );
}

async fn the_daemon_console_and_tasks(test_db: &TestDatabase) {
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_worker`.worker_taskdata (id, data) VALUES (1, '17'); \
             INSERT INTO `{{ns}}_worker`.worker_activetask (id, taskClass, leaseOwner, \
               leaseExpires, failureCount, dataID, priority, dateCreated, dateModified) VALUES \
             (3, 'PhabricatorMetaMTAWorker', '1:2:3', {lease}, 2, 1, 2000, 1, 1), \
             (4, 'FeedPublisherWorker', NULL, NULL, 0, 1, 1000, 1, 1); \
             INSERT INTO `{{ns}}_worker`.worker_archivetask (id, taskClass, leaseOwner, \
               leaseExpires, failureCount, dataID, priority, result, duration, dateCreated, \
               dateModified, archivedEpoch) VALUES \
             (2, 'PhabricatorSearchWorker', NULL, NULL, 0, 1, 2000, 0, 1500, {recent}, \
               {recent}, {recent})",
            lease = NOW + 125,
            recent = NOW - 60,
        ))
        .await;
    let console = page(test_db, "/daemon/", ROOT).await;
    assert!(
        console.contains("<td>PhabricatorSearchWorker</td><td>1</td><td>1,500 us</td><td>0 s</td>"),
        "{console}"
    );
    assert!(console.contains("<td>PhabricatorMetaMTAWorker</td><td>1</td>"));
    assert!(
        console.contains("<td>3</td><td>PhabricatorMetaMTAWorker</td><td>1:2:3</td><td>2 m</td>")
    );
    assert!(console.contains("There are no upcoming event triggers."));

    let task = page(test_db, "/daemon/task/3/", ROOT).await;
    assert!(
        task.contains("<dt>Task Status</dt><dd>Queued</dd>"),
        "{task}"
    );
    assert!(task.contains("<dt>Lease Status</dt><dd>Leased</dd>"));
    assert!(task.contains("<dt>Lease Expires</dt><dd>2 m, 5 s</dd>"));
    assert!(task.contains("./bin/mail show-outbound --id 17"));
    assert!(task.contains("<dt>Maximum Retries</dt><dd>250</dd>"));
    assert!(task.contains("<dt>Retries After</dt><dd>30 s, 1 m, 2 m"));
    let archived = page(test_db, "/daemon/task/2/", ROOT).await;
    assert!(archived.contains("<dt>Task Status</dt><dd>Complete</dd>"));
    assert!(archived.contains("<dt>Duration</dt><dd>1,500 us</dd>"));
    let missing = page(test_db, "/daemon/task/99/", ROOT).await;
    assert!(missing.contains("Task 99 Missing"));

    let bulk = page(test_db, "/daemon/bulk/query/all/", ALICE).await;
    assert!(bulk.contains("No bulk jobs found."));
}

async fn conduit_methods(test_db: &TestDatabase) {
    let list = page(test_db, "/conduit/", ALICE).await;
    assert!(list.contains("<h2>conduit</h2>"), "{list}");
    assert!(list.contains("<a href=\"/conduit/method/conduit.ping/\">conduit.ping</a>"));
    let modern_has_deprecated = list.contains("<span class=\"tag\">Deprecated</span>");
    let all = page(test_db, "/conduit/query/all/", ALICE).await;
    assert!(all.contains("<span class=\"tag\">Deprecated</span>"));
    assert!(!modern_has_deprecated);

    let method = page(test_db, "/conduit/method/conduit.ping/", ALICE).await;
    assert!(method.contains("API Method: conduit.ping"), "{method}");
    assert!(method.contains("<dt>Returns</dt><dd>string</dd>"));
    assert!(method.contains("<strong>ERR-CONDUIT-CORE:</strong>"));
    assert!(method.contains("href=\"/rust-ui/off?next=%2Fconduit%2Fmethod%2Fconduit.ping%2F\""));
    let response = get(
        test_db,
        "/conduit/method/differential.revision.search/",
        ALICE,
    )
    .await;
    assert_eq!(
        fallback_reason(&response),
        "a method with documentation pages"
    );
    let response = get(test_db, "/conduit/method/no.such.method/", ALICE).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

async fn spaces_mail_help_and_search(test_db: &TestDatabase) {
    let spaces = page(test_db, "/spaces/", ALICE).await;
    assert!(spaces.contains("No spaces found."), "{spaces}");
    test_db
        .execute(
            "INSERT INTO `{ns}_spaces`.spaces_namespace (id, phid, namespaceName, viewPolicy, \
               editPolicy, isDefaultNamespace, dateCreated, dateModified, description, \
               isArchived) VALUES \
             (1, 'PHID-SPCE-1', 'Everyone', 'users', 'admin', 1, 1, 1, 'For **all**.', 0); \
             INSERT INTO `{ns}_metamta`.metamta_mail (id, phid, actorPHID, parameters, status, \
               message, relatedPHID, dateCreated, dateModified) VALUES \
             (1, 'PHID-MTAM-1', 'PHID-USER-root', \
               '{\"to\":[\"PHID-USER-alice\"],\"subject\":\"Hi\",\"sensitive\":false,\
                 \"body\":\"Hello <you>\",\"headers.sent\":[[\"X-B\",\"2\"],[\"X-A\",\"1\"]],\
                 \"actors.sent\":{\"PHID-USER-alice\":{\"deliverable\":true,\"reasons\":[]}}}', \
               'sent', NULL, NULL, 1, 1), \
             (2, 'PHID-MTAM-2', 'PHID-USER-root', '{\"to\":[\"PHID-USER-root\"]}', 'queued', \
               NULL, NULL, 1, 1)",
        )
        .await;
    let space = page(test_db, "/S1", ALICE).await;
    assert!(
        space.contains("<dt>Default Space</dt><dd>Yes</dd>"),
        "{space}"
    );
    assert!(space.contains("For <strong>all</strong>."));
    assert!(space.contains("<span class=\"hm-disabled\" aria-disabled=\"true\"><span class=\"fa fa-pencil\" aria-hidden=\"true\"></span> Edit Space</span>"));

    let mail = page(test_db, "/mail/detail/1/", ALICE).await;
    assert!(mail.contains("Hi</h1>"), "{mail}");
    assert!(mail.contains("<pre class=\"remarkup-wrap\">Hello &lt;you&gt;</pre>"));
    assert!(mail.contains("<dt>X-A</dt><dd>1</dd>\n    <dt>X-B</dt><dd>2</dd>"));
    assert!(mail.contains("<dt>Delivery</dt><dd>Delivered</dd>"));
    assert!(mail.contains("<strong>None</strong> No special rules affected this mail."));
    let response = get(test_db, "/mail/detail/2/", ALICE).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let redacted = page(test_db, "/mail/detail/2/", ROOT).await;
    assert!(redacted.contains("Content Redacted</h1>"));

    let help = page(
        test_db,
        "/help/documentation/PhabricatorDifferentialApplication/",
        ALICE,
    )
    .await;
    assert!(help.contains(">Differential User Guide</a>"), "{help}");
    let keys = page(
        test_db,
        "/help/keyboardshortcut/?keys=%5B%7B%22keys%22%3A%5B%22j%22%5D%2C%22description%22%3A%22Next%22%7D%5D",
        ALICE,
    )
    .await;
    assert!(
        keys.contains("<td><kbd>j</kbd></td><td>Next</td>"),
        "{keys}"
    );
    assert!(keys.contains("<h2>Global Shortcuts</h2>"));

    let response = get(test_db, "/search/?query=f", ALICE).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/feed/");
    let response = get(test_db, "/search/?query=D5", ALICE).await;
    assert_eq!(fallback_reason(&response), "a fulltext search");
    let response = get(test_db, "/search/", ALICE).await;
    assert_eq!(fallback_reason(&response), "the search page");
}

async fn the_home_page_and_its_menu(test_db: &TestDatabase) {
    let home = page(test_db, "/", ALICE).await;
    assert!(home.contains("<h2>Recent Activity</h2>"), "{home}");
    assert!(home.contains("<h2>Active Repositories</h2>"));
    assert!(home.contains("<a href=\"/applications/\"><span class=\"fa fa-ellipsis-h\" aria-hidden=\"true\"></span> More Applications</a>"));
    assert!(home.contains("<a href=\"/differential/\">Differential</a>"));
    // Config is pinned for administrators only.
    assert!(!home.contains("<a href=\"/config/\">Config</a>"));
    let admin_home = page(test_db, "/", ROOT).await;
    assert!(
        admin_home.contains("<a href=\"/config/\">Config</a>"),
        "{admin_home}"
    );

    test_db
        .execute(
            "INSERT INTO `{ns}_search`.search_profilepanelconfiguration (phid, profilePHID, \
               menuItemKey, builtinKey, menuItemOrder, visibility, menuItemProperties, \
               customPHID, dateCreated, dateModified) VALUES \
             ('PHID-PANL-1', 'PHID-APPS-PhabricatorHomeApplication', 'dashboard', NULL, 1, \
               'default', '{}', NULL, 1, 1)",
        )
        .await;
    let response = get(test_db, "/", ALICE).await;
    assert_eq!(fallback_reason(&response), "a customized home menu");
}

/// One set of databases for every page: creating them is what makes
/// these tests slow.
#[tokio::test]
async fn system_pages() {
    let Some(test_db) = setup().await else {
        return;
    };
    applications_list_and_detail(&test_db).await;
    policies_of_applications_are_explained(&test_db).await;
    configuration_is_for_administrators(&test_db).await;
    the_daemon_console_and_tasks(&test_db).await;
    conduit_methods(&test_db).await;
    spaces_mail_help_and_search(&test_db).await;
    the_home_page_and_its_menu(&test_db).await;
    test_db.drop_databases().await;
}
