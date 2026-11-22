//! The settings pages against a test database: what each panel shows, who
//! may see it, and the rows a save writes.

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::{FALLBACK_HEADER, WebState};
use serde_json::{Value, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
const ALICE: &str = "phsid=alice-token; phab_ui=rust";
const ADMIN: &str = "phsid=admin-token; phab_ui=rust";

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

/// Users `alice`, `bob`, `admin` (an administrator) and `bot`, with
/// sessions `alice-token` and `admin-token`, and cached preferences for
/// `alice` and `bob`.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "user", "auth", "worker", "herald", "conduit", "spaces", "project",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-admin', 'admin', 'Admin', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0), \
             ('PHID-USER-bot', 'bot', 'Bot', 1, 1, 'c', 1, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial, \
                highSecurityUntil) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1699000000, {expires}, 'PHID-SSSN-1', 0, NULL), \
             ('PHID-USER-admin', 'web', '{admin}', 1, {expires}, 'PHID-SSSN-2', 0, NULL), \
             ('PHID-USER-alice', 'conduit', 'abcdef0123', 1699500000, {expires}, 'PHID-SSSN-3', \
              0, {hisec}); \
             INSERT INTO `{{ns}}_user`.user_cache (userPHID, cacheIndex, cacheKey, cacheData, \
               cacheType) VALUES \
             ('PHID-USER-alice', '{preferences}', 'user.preferences.v2', '[]', 'preferences'), \
             ('PHID-USER-alice', '{other}', 'user.other', 'x', 'other'), \
             ('PHID-USER-bob', '{preferences}', 'user.preferences.v2', '[]', 'preferences')",
            alice = session_key("alice-token"),
            admin = session_key("admin-token"),
            expires = NOW + 29 * 86_400,
            hisec = NOW + 300,
            preferences = hash::digest_for_index(b"user.preferences.v2"),
            other = hash::digest_for_index(b"user.other"),
        ))
        .await;
    Some(test_db)
}

async fn send(
    test_db: &TestDatabase,
    config: Config,
    request: axum::http::request::Builder,
    body: Body,
) -> Response {
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn get(test_db: &TestDatabase, path: &str, cookie: &str) -> Response {
    send(
        test_db,
        Config::default(),
        Request::get(path).header(header::COOKIE, cookie),
        Body::empty(),
    )
    .await
}

async fn post(test_db: &TestDatabase, path: &str, pairs: &[(&str, &str)]) -> Response {
    let page = text(get(test_db, path, ALICE).await).await;
    let token = csrf_token(&page);
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("__csrf__", &token)
        .extend_pairs(pairs)
        .finish();
    send(
        test_db,
        Config::default(),
        Request::post(path)
            .header(header::COOKIE, ALICE)
            .header(header::HOST, "phabricator.test")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Body::from(body),
    )
    .await
}

fn csrf_token(page: &str) -> String {
    page.split("name=\"csrf-token\" content=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned()
}

async fn text(response: Response) -> String {
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn fallback_reason(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    response.headers()[FALLBACK_HEADER].to_str().unwrap()
}

fn location(response: &Response) -> &str {
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

/// The preferences rows: user, settings, dates and the PHID type.
async fn preferences(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', IFNULL(userPHID, '-'), preferences, dateCreated, dateModified, \
           LEFT(phid, 10), IFNULL(builtinKey, '-')) AS line \
         FROM `{ns}_user`.user_preferences ORDER BY id",
    )
    .await
}

/// The transactions: object type, values, metadata without the group ID
/// and the columns they all share.
async fn transactions(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', LEFT(t.objectPHID, 10), t.objectPHID = p.phid, t.transactionType, \
           t.oldValue, t.newValue, \
           REPLACE(t.metadata, SUBSTRING(t.metadata, LOCATE('core.groupID', t.metadata) + 15, \
             32), 'G'), \
           t.authorPHID, t.viewPolicy, t.editPolicy, IFNULL(t.commentPHID, '-'), \
           t.commentVersion, t.contentSource, t.dateCreated, t.dateModified, LEFT(t.phid, 15)) \
           AS line \
         FROM `{ns}_user`.user_preferencestransaction t \
         JOIN `{ns}_user`.user_preferences p ON p.phid = t.objectPHID ORDER BY t.id",
    )
    .await
}

/// The group IDs of the transactions, which a save shares.
async fn group_ids(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT SUBSTRING(metadata, LOCATE('core.groupID', metadata) + 15, 32) AS line \
         FROM `{ns}_user`.user_preferencestransaction ORDER BY id",
    )
    .await
}

async fn caches(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', userPHID, cacheKey) AS line FROM `{ns}_user`.user_cache \
         ORDER BY userPHID, cacheKey",
    )
    .await
}

/// The queued tasks: class, priority, whether the object is the
/// preferences row, and the data.
async fn tasks(test_db: &TestDatabase) -> Vec<(String, Value)> {
    lines(
        test_db,
        "SELECT CONCAT(t.taskClass, ' ', t.priority, ' ', \
           t.objectPHID = (SELECT phid FROM `{ns}_user`.user_preferences WHERE userPHID = \
             'PHID-USER-alice'), ' ', d.data) AS line \
         FROM `{ns}_worker`.worker_activetask t JOIN `{ns}_worker`.worker_taskdata d \
           ON d.id = t.dataID ORDER BY t.id",
    )
    .await
    .into_iter()
    .map(|line| {
        let (head, data) = line.split_at(line.find('{').expect("JSON"));
        (
            head.trim_end().to_owned(),
            serde_json::from_str(data).unwrap(),
        )
    })
    .collect()
}

async fn preferences_phid(test_db: &TestDatabase) -> String {
    lines(
        test_db,
        "SELECT phid AS line FROM `{ns}_user`.user_preferences WHERE userPHID = 'PHID-USER-alice'",
    )
    .await
    .remove(0)
}

async fn transaction_phids(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT phid AS line FROM `{ns}_user`.user_preferencestransaction ORDER BY id",
    )
    .await
}

/// The publish task PHP's preferences editor queues.
fn publish(object: &str, xactions: &[String], is_new: bool) -> Value {
    json!({
        "objectPHID": object,
        "actorPHID": "PHID-USER-alice",
        "xactionPHIDs": xactions,
        "state": {
            "parentMessageID": null,
            "isNewObject": is_new,
            "heraldEmailPHIDs": [],
            "heraldForcedEmailPHIDs": [],
            "heraldHeader": null,
            "mailToPHIDs": [],
            "mailCCPHIDs": [],
            "feedNotifyPHIDs": [],
            "feedRelatedPHIDs": [],
            "feedShouldPublish": false,
            "mailShouldSend": false,
            "mustEncrypt": [],
            "mailStamps": [],
            "mailUnexpandablePHIDs": [],
            "mailMutedPHIDs": [],
            "webhookMap": [],
            "silent": null,
            "sendHistory": false,
            "excludeMailRecipientPHIDs": [],
            "custom": [],
            "custom.encoding": [],
        },
    })
}

/// The columns every transaction of alice's saves shares.
fn shared() -> String {
    format!(
        "PHID-USER-alice public PHID-USER-alice - 0 {{\"source\":\"web\",\"params\":[]}} {NOW} \
         {NOW} PHID-XACT-PSET-"
    )
}

#[tokio::test]
async fn the_settings_go_to_the_viewers_own() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = get(&test_db, "/settings/", ALICE).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(location(&response), "/settings/user/alice/");
    // Administrators get the list of every user's settings.
    let response = get(&test_db, "/settings/", ADMIN).await;
    assert_eq!(fallback_reason(&response), "the settings of every user");
    let response = get(&test_db, "/settings/panel/email%20x/", ALICE).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(location(&response), "/settings/user/alice/page/email%20x/");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn settings_are_their_owners() {
    let Some(test_db) = setup().await else {
        return;
    };
    for path in ["/settings/user/bob/", "/settings/user/bob/page/sessions/"] {
        let response = get(&test_db, path, ALICE).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let response = get(&test_db, "/settings/user/nobody/page/datetime/", ALICE).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // Administrators edit bots, in PHP.
    let response = get(&test_db, "/settings/user/bot/page/datetime/", ADMIN).await;
    assert_eq!(
        fallback_reason(&response),
        "administrators editing the settings of a bot or mailing list"
    );
    let response = get(&test_db, "/settings/user/bot/page/datetime/", ALICE).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Administrators may not see other users' settings either.
    let response = get(&test_db, "/settings/user/alice/page/datetime/", ADMIN).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A post as someone else writes nothing.
    let response = send(
        &test_db,
        Config::default(),
        Request::post("/settings/user/bob/page/datetime/")
            .header(header::COOKIE, ALICE)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Body::from("__form__=1&editEngine=true&timezone=UTC"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(preferences(&test_db).await, Vec::<String>::new());
    let response = get(
        &test_db,
        "/settings/user/alice/page/datetime/",
        "phab_ui=rust",
    )
    .await;
    assert_eq!(fallback_reason(&response), "logged out");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn the_first_panel_is_date_and_time() {
    let Some(test_db) = setup().await else {
        return;
    };
    for path in [
        "/settings/user/alice/",
        "/settings/user/alice/page/datetime/",
        "/settings/user/alice/page/unknown/",
    ] {
        let response = get(&test_db, path, ALICE).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let page = text(response).await;
        assert!(page.contains("<h1>Edit Settings: alice</h1>"), "{page}");
        assert!(page.contains("<h2>Date and Time</h2>"), "{page}");
        assert!(
            page.contains(r#"<a href="/settings/user/alice/page/datetime/" aria-current="page">"#),
            "{page}"
        );
        for panel in [
            "language",
            "diff",
            "display",
            "editor",
            "external",
            "multifactor",
            "ssh",
            "email",
            "emaildelivery",
            "emailformat",
            "emailpreferences",
            "activity",
            "apitokens",
            "sessions",
            "tokens",
        ] {
            assert!(
                page.contains(&format!(r#"href="/settings/user/alice/page/{panel}/""#)),
                "{panel}: {page}"
            );
        }
        // No password provider, no HTTP access to repositories.
        assert!(!page.contains("/page/password/"));
        assert!(!page.contains("/page/vcspassword/"));
    }
    let page = text(get(&test_db, "/settings/user/alice/", ALICE).await).await;
    assert!(page.contains(r#"<option value="" selected>Default (UTC)</option>"#));
    assert!(page.contains(r#"<optgroup label="UTC+1">"#), "{page}");
    assert!(page.contains(r#"<option value="Europe/Paris">Europe/Paris</option>"#));
    assert!(page.contains(r#"<option value="" selected>Default (ISO 8601: 2000-02-28)</option>"#));
    assert!(page.contains(r#"<option value="" selected>Default (12 Hour, 2:34 PM)</option>"#));
    assert!(page.contains(r#"<option value="" selected>Default (Sunday)</option>"#));
    assert!(page.contains("Select your local timezone."));
    assert!(page.contains(r#"name="editEngine" value="true""#));
    test_db.drop_databases().await;
}

#[tokio::test]
async fn preference_panels_show_the_stored_values_and_global_defaults() {
    let Some(test_db) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user_preferences (userPHID, preferences, dateCreated, \
               dateModified, phid, builtinKey) VALUES \
             ('PHID-USER-alice', '{\"pronoun\":\"f\",\"monospaced\":\"13px Fira <b>\",\
               \"titles\":\"text\",\"week-start-day\":1}', 1, 1, 'PHID-PSET-alice', NULL), \
             (NULL, '{\"no-mail\":\"2\",\"html-emails\":\"text\"}', 1, 1, 'PHID-PSET-global', \
               'global')",
        )
        .await;
    let page = text(get(&test_db, "/settings/user/alice/page/datetime/", ALICE).await).await;
    assert!(
        page.contains(r#"<option value="1" selected>Monday</option>"#),
        "{page}"
    );
    let page = text(get(&test_db, "/settings/user/alice/page/language/", ALICE).await).await;
    assert!(page.contains(r#"<option value="" selected>Default (English (US))</option>"#));
    assert!(page.contains(r#"<optgroup label="Translations">"#));
    assert!(page.contains(r#"<option value="f" selected>alice updated her profile</option>"#));
    assert!(page.contains(r#"<option value="">alice updated their profile</option>"#));
    let page = text(get(&test_db, "/settings/user/alice/page/display/", ALICE).await).await;
    assert!(page.contains(r#"value="13px Fira &#60;b&#62;""#), "{page}");
    assert!(
        page.contains(r#"<option value="text" selected>Use Plain Text: [Differential]</option>"#)
    );
    assert!(page.contains(r#"<option value="" selected>Default (Use Standard Colors)</option>"#));
    let page = text(get(&test_db, "/settings/user/alice/page/diff/", ALICE).await).await;
    assert!(page.contains("Show Unified Diffs") && page.contains("Show Older Inlines"));
    // The global defaults name the default.
    let page = text(get(&test_db, "/settings/user/alice/page/emaildelivery/", ALICE).await).await;
    assert!(
        page.contains(r#"<option value="" selected>Default (Mozilla Notifications)</option>"#),
        "{page}"
    );
    assert!(
        page.contains("<strong>Mozilla Notifications</strong>"),
        "{page}"
    );
    let page = text(get(&test_db, "/settings/user/alice/page/emailformat/", ALICE).await).await;
    assert!(page.contains(r#"<option value="" selected>Default (Send Plain Text Email)</option>"#));
    assert!(
        page.contains(
            r#"<option value="" selected>Default (Disable &#34;Re:&#34; Prefix)</option>"#
        ),
        "{page}"
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn saving_a_panel_creates_the_preferences() {
    let Some(test_db) = setup().await else {
        return;
    };
    let response = post(
        &test_db,
        "/settings/user/alice/page/datetime/",
        &[
            ("__form__", "1"),
            ("editEngine", "true"),
            ("timezone", "Europe/Paris"),
            ("date-format", ""),
            ("time-format", "H:i"),
            ("week-start-day", ""),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(location(&response), "/settings/user/alice/page/datetime/");
    assert_eq!(
        response.headers()[header::SET_COOKIE],
        "phfrm=edit-engine%2Fsettings.settings; path=/; domain=phabricator.test; HttpOnly"
    );
    assert_eq!(
        preferences(&test_db).await,
        [format!(
            r#"PHID-USER-alice {{"timezone":"Europe\/Paris","time-format":"H:i"}} {NOW} {NOW} PHID-PSET- -"#
        )]
    );
    assert_eq!(
        transactions(&test_db).await,
        [
            format!(
                r#"PHID-PSET- 1 setting null "Europe\/Paris" {{"setting.key":"timezone","core.groupID":"G"}} {}"#,
                shared()
            ),
            format!(
                r#"PHID-PSET- 1 setting null "H:i" {{"setting.key":"time-format","core.groupID":"G"}} {}"#,
                shared()
            ),
        ]
    );
    let groups = group_ids(&test_db).await;
    assert_eq!(groups[0].len(), 32);
    assert_eq!(groups[0], groups[1]);
    // Only alice's cached preferences are gone.
    assert_eq!(
        caches(&test_db).await,
        [
            "PHID-USER-alice user.other",
            "PHID-USER-bob user.preferences.v2"
        ]
    );
    let object = preferences_phid(&test_db).await;
    assert_eq!(
        tasks(&test_db).await,
        [(
            "PhabricatorApplicationTransactionPublishWorker 1000 1".to_owned(),
            publish(&object, &transaction_phids(&test_db).await, true)
        )]
    );

    // The next page says the changes were saved, and forgets it.
    let response = get(
        &test_db,
        "/settings/user/alice/page/datetime/",
        &format!("{ALICE}; phfrm=edit-engine%2Fsettings.settings"),
    )
    .await;
    let page = text(response).await;
    assert!(page.contains("Changes saved."), "{page}");
    assert!(page.contains(r#"<option value="Europe/Paris" selected>Europe/Paris</option>"#));
    assert!(page.contains(r#"<option value="H:i" selected>24 Hour, 14:34</option>"#));
    let page = text(get(&test_db, "/settings/user/alice/page/datetime/", ALICE).await).await;
    assert!(!page.contains("Changes saved."));
    test_db.drop_databases().await;
}

#[tokio::test]
async fn saving_a_panel_updates_the_preferences() {
    let Some(test_db) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user_preferences (userPHID, preferences, dateCreated, \
               dateModified, phid, builtinKey) VALUES ('PHID-USER-alice', \
               '{\"timezone\":\"Europe\\/Paris\",\"time.offset.ignore\":\"x\",\
               \"week-start-day\":1,\"titles\":\"\"}', 5, 6, 'PHID-PSET-alice', NULL)",
        )
        .await;
    let response = post(
        &test_db,
        "/settings/user/alice/page/datetime/",
        &[
            ("__form__", "1"),
            ("editEngine", "true"),
            ("timezone", "Europe/Paris"),
            ("date-format", "d-m-Y"),
            ("time-format", ""),
            ("week-start-day", "1"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    // The ignored offset goes with any time zone change, and the stored
    // number becomes a string.
    assert_eq!(
        preferences(&test_db).await,
        [format!(
            r#"PHID-USER-alice {{"timezone":"Europe\/Paris","week-start-day":"1","titles":"","date-format":"d-m-Y"}} 5 {NOW} PHID-PSET- -"#
        )]
    );
    assert_eq!(
        transactions(&test_db).await,
        [
            format!(
                r#"PHID-PSET- 1 setting "x" null {{"setting.key":"time.offset.ignore","core.groupID":"G"}} {}"#,
                shared()
            ),
            format!(
                r#"PHID-PSET- 1 setting null "d-m-Y" {{"setting.key":"date-format","core.groupID":"G"}} {}"#,
                shared()
            ),
            format!(
                r#"PHID-PSET- 1 setting 1 "1" {{"setting.key":"week-start-day","core.groupID":"G"}} {}"#,
                shared()
            ),
        ]
    );
    assert_eq!(
        tasks(&test_db).await,
        [(
            "PhabricatorApplicationTransactionPublishWorker 1000 1".to_owned(),
            publish("PHID-PSET-alice", &transaction_phids(&test_db).await, false)
        )]
    );

    // An empty stored value the form sends back is a default transaction
    // which removes it.
    let response = post(
        &test_db,
        "/settings/user/alice/page/display/",
        &[
            ("__form__", "1"),
            ("editEngine", "true"),
            ("resource-postprocessor", ""),
            ("titles", ""),
            ("monospaced", ""),
            ("monospaced-textareas", ""),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        transactions(&test_db).await[3],
        format!(
            r#"PHID-PSET- 1 setting "" null {{"setting.key":"titles","core.default":true,"core.groupID":"G"}} {}"#,
            shared()
        )
    );
    assert_eq!(
        preferences(&test_db).await,
        [format!(
            r#"PHID-USER-alice {{"timezone":"Europe\/Paris","week-start-day":"1","date-format":"d-m-Y"}} 5 {NOW} PHID-PSET- -"#
        )]
    );

    // Nothing to change still saves the row and queues the task.
    let response = post(
        &test_db,
        "/settings/user/alice/page/emaildelivery/",
        &[
            ("__form__", "1"),
            ("editEngine", "true"),
            ("no-mail", ""),
            ("self-mail", ""),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(transactions(&test_db).await.len(), 4);
    let tasks = tasks(&test_db).await;
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[2].1, publish("PHID-PSET-alice", &[], false));
    test_db.drop_databases().await;
}

#[tokio::test]
async fn saves_php_refuses_go_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    for (path, field, value) in [
        (
            "/settings/user/alice/page/datetime/",
            "timezone",
            "Mars/Olympus",
        ),
        ("/settings/user/alice/page/datetime/", "week-start-day", "7"),
        (
            "/settings/user/alice/page/display/",
            "monospaced",
            "13px; color: red",
        ),
    ] {
        let response = post(
            &test_db,
            path,
            &[("__form__", "1"), ("editEngine", "true"), (field, value)],
        )
        .await;
        assert_eq!(
            fallback_reason(&response),
            "settings PHP refuses",
            "{field}"
        );
    }
    assert_eq!(preferences(&test_db).await, Vec::<String>::new());
    assert_eq!(tasks(&test_db).await, Vec::new());

    // A post without the form markers only shows the form.
    let response = post(
        &test_db,
        "/settings/user/alice/page/datetime/",
        &[("timezone", "UTC")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // A wrong CSRF token is refused.
    let response = send(
        &test_db,
        Config::default(),
        Request::post("/settings/user/alice/page/datetime/")
            .header(header::COOKIE, ALICE)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Body::from("__csrf__=nope&__form__=1&editEngine=true&timezone=UTC"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(preferences(&test_db).await, Vec::<String>::new());

    // Developer mode offers translations Rust doesn't list.
    let mut config = Config::default();
    config.push_layer(
        "test",
        serde_json::Map::from_iter([("phabricator.developer-mode".to_owned(), json!(true))]),
    );
    let response = send(
        &test_db,
        config,
        Request::get("/settings/user/alice/page/language/").header(header::COOKIE, ALICE),
        Body::empty(),
    )
    .await;
    assert_eq!(
        fallback_reason(&response),
        "the translations of developer mode"
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn mail_tags_are_shown_and_saved() {
    let Some(test_db) = setup().await else {
        return;
    };
    let page = text(
        get(
            &test_db,
            "/settings/user/alice/page/emailpreferences/",
            ALICE,
        )
        .await,
    )
    .await;
    assert!(page.contains("<legend>Audits</legend>"), "{page}");
    assert!(page.contains("<legend>Differential Revisions</legend>"));
    assert!(page.contains("<legend>Projects</legend>"));
    // Calendar is a prototype, not installed.
    assert!(!page.contains("<legend>Calendar</legend>"));
    assert!(page.contains(r#"<select name="mailtags[differential-cc]""#));
    assert!(page.contains(r#"<option value="1" selected>"#));
    assert!(page.contains("Herald Rules"));

    let response = post(
        &test_db,
        "/settings/user/alice/page/emailpreferences/",
        &[
            ("__form__", "1"),
            ("mailtags[differential-cc]", "2"),
            ("mailtags[project-members]", "0"),
            ("mailtags[calendar-other]", "2"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        location(&response),
        "/settings/user/alice/page/emailpreferences/?saved=true"
    );
    let expected = r#"{"differential-review-request":1,"differential-updated":1,"differential-comment":1,"differential-committed":1,"differential-reviewers":1,"differential-cc":2,"differential-other":1,"audit-commit":1,"audit-action-concern":1,"audit-action-accept":1,"audit-action-resign":1,"audit-action-close":1,"audit-add-auditors":1,"audit-add-ccs":1,"audit-projects":1,"audit-comment":1,"audit-other":1,"project-metadata":1,"project-members":0,"project-watchers":1,"project-other":1}"#;
    assert_eq!(
        preferences(&test_db).await,
        [format!(
            r#"PHID-USER-alice {{"mailtags":{expected}}} {NOW} {NOW} PHID-PSET- -"#
        )]
    );
    assert_eq!(
        transactions(&test_db).await,
        [format!(
            r#"PHID-PSET- 1 setting null {expected} {{"setting.key":"mailtags","core.groupID":"G"}} {}"#,
            shared()
        )]
    );
    assert_eq!(caches(&test_db).await.len(), 2);
    let object = preferences_phid(&test_db).await;
    assert_eq!(
        tasks(&test_db).await,
        [(
            "PhabricatorApplicationTransactionPublishWorker 1000 1".to_owned(),
            publish(&object, &transaction_phids(&test_db).await, true)
        )]
    );
    let page = text(
        get(
            &test_db,
            "/settings/user/alice/page/emailpreferences/?saved=true",
            ALICE,
        )
        .await,
    )
    .await;
    assert!(
        page.contains(
            "<select name=\"mailtags[differential-cc]\" aria-label=\"A revision&#39;s CCs change.\">"
        ),
        "{page}"
    );

    // Saving the same tags again has no effect.
    let response = post(
        &test_db,
        "/settings/user/alice/page/emailpreferences/",
        &[
            ("__form__", "1"),
            ("mailtags[differential-cc]", "2"),
            ("mailtags[project-members]", "0"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(transactions(&test_db).await.len(), 1);
    assert_eq!(tasks(&test_db).await.len(), 2);
    test_db.drop_databases().await;
}

/// Addresses, Conduit tokens and factors of alice and bob.
async fn insert_list_fixtures(test_db: &TestDatabase) {
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user_email (phid, userPHID, address, isVerified, isPrimary, \
               verificationCode, dateCreated, dateModified) VALUES \
             ('PHID-EADR-1', 'PHID-USER-alice', 'b@example.com', 0, 0, 'v', 1, 1), \
             ('PHID-EADR-2', 'PHID-USER-alice', 'a@example.com', 1, 1, 'v', 1, 1), \
             ('PHID-EADR-3', 'PHID-USER-alice', 'c@example.com', 1, 0, 'v', 1, 1), \
             ('PHID-EADR-4', 'PHID-USER-bob', 'bob@example.com', 1, 1, 'v', 1, 1); \
             INSERT INTO `{{ns}}_conduit`.conduit_token (objectPHID, tokenType, token, expires, \
               dateCreated, dateModified) VALUES \
             ('PHID-USER-alice', 'api', 'api-abcdefghijklmnop', NULL, 1699000000, 1), \
             ('PHID-USER-alice', 'cli', 'cli-qrstuvwxyz', {expired}, 1, 1), \
             ('PHID-USER-alice', 'cli', 'cli-0123456789', {later}, 1699000000, 1), \
             ('PHID-USER-bob', 'api', 'api-bobbobbob', NULL, 1, 1); \
             INSERT INTO `{{ns}}_auth`.auth_factorprovider (phid, providerFactorKey, status, \
               properties, dateCreated, dateModified, name) VALUES \
             ('PHID-FPRV-1', 'totp', 'deprecated', '{{}}', 1, 1, ''), \
             ('PHID-FPRV-2', 'totp', 'active', '{{}}', 1, 1, 'Phone'); \
             INSERT INTO `{{ns}}_auth`.auth_factorconfig (phid, userPHID, factorName, \
               factorSecret, properties, dateCreated, dateModified, factorProviderPHID) VALUES \
             ('PHID-AFTR-1', 'PHID-USER-alice', 'Old phone', '0123456789', '{{}}', 1699000000, \
               1, 'PHID-FPRV-1'), \
             ('PHID-AFTR-2', 'PHID-USER-alice', 'New phone', '0123456789abcdef', '{{}}', \
               1699000000, 1, 'PHID-FPRV-2'), \
             ('PHID-AFTR-3', 'PHID-USER-bob', 'Bob phone', 'x', '{{}}', 1, 1, 'PHID-FPRV-2')",
            expired = NOW - 1,
            later = NOW + 3600,
        ))
        .await;
}

#[tokio::test]
async fn list_panels_show_the_users_own() {
    let Some(test_db) = setup().await else {
        return;
    };
    insert_list_fixtures(&test_db).await;

    let page = text(get(&test_db, "/settings/user/alice/page/email/", ALICE).await).await;
    let a = page.find("a@example.com").unwrap();
    let b = page.find("b@example.com").unwrap();
    let c = page.find("c@example.com").unwrap();
    assert!(a < b && b < c, "{page}");
    assert!(!page.contains("bob@example.com"));
    assert!(page.contains(r#"?verify=1">Verify</a>"#), "{page}");
    assert!(page.contains(r#"?primary=3">Make Primary</a>"#), "{page}");
    assert!(page.contains(r#"?delete=1">Remove</a>"#));
    assert!(page.contains("Add New Address"));
    let mut config = Config::default();
    config.push_layer(
        "test",
        serde_json::Map::from_iter([("account.editable".to_owned(), json!(false))]),
    );
    let response = send(
        &test_db,
        config,
        Request::get("/settings/user/alice/page/email/").header(header::COOKIE, ALICE),
        Body::empty(),
    )
    .await;
    let page = text(response).await;
    assert!(!page.contains("Add New Address") && !page.contains("Remove"));

    let page = text(get(&test_db, "/settings/user/alice/page/apitokens/", ALICE).await).await;
    assert!(page.contains(">api-abcd...</a>"), "{page}");
    assert!(page.contains("Standard API Token"));
    assert!(page.contains(">cli-0123...</a>"));
    assert!(!page.contains("cli-qrst") && !page.contains("api-bobb"));
    assert!(page.contains("<td>Never</td>"));
    assert!(page.contains("/conduit/token/edit/?objectPHID=PHID-USER-alice"));

    let page = text(
        get(
            &test_db,
            "/settings/user/alice/page/multifactor/?id=2",
            ALICE,
        )
        .await,
    )
    .await;
    let new = page.find("New phone").unwrap();
    let old = page.find("Old phone").unwrap();
    // Active providers first.
    assert!(new < old, "{page}");
    assert!(page.contains("<td>128-Bit Secret</td>"), "{page}");
    assert!(page.contains("<td>Phone</td>") && page.contains("<td>Mobile Phone App (TOTP)</td>"));
    assert!(page.contains(r#"<tr class="highlighted">"#));
    assert!(!page.contains("Bob phone"));
    assert!(page.contains("?new=true"));

    let page = text(get(&test_db, "/settings/user/alice/page/sessions/", ALICE).await).await;
    let conduit = page.find("<td>conduit</td>").unwrap();
    let web = page.find("<td>web</td>").unwrap();
    assert!(conduit < web, "{page}");
    assert!(page.contains("<code>abcdef</code>"));
    assert!(page.contains("<td>5 m</td>"), "{page}");
    assert!(page.contains(r#"<span class="button disabled">Current</span>"#));
    assert!(page.contains(r#"href="/auth/session/terminate/3/""#));
    assert!(!page.contains("Leave High Security"));

    // Their actions are PHP's.
    for path in [
        "/settings/user/alice/page/email/?new=true",
        "/settings/user/alice/page/email/?delete=1",
        "/settings/user/alice/page/multifactor/?new=true",
        "/settings/user/alice/page/multifactor/?edit=2",
        "/settings/user/alice/page/ssh/",
        "/settings/user/alice/page/external/",
        "/settings/user/alice/page/apitokens/?__ajax__=1",
    ] {
        let response = get(&test_db, path, ALICE).await;
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    }
    test_db.drop_databases().await;
}
