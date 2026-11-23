//! `dashboard.panel.edit` and `portal.edit` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-dededededededededededededede";
const NOW: i64 = 1_700_000_000;

const SEED: &str = "\
    INSERT INTO `{ns}_user`.user (phid, userName, realName, dateCreated, dateModified, \
      conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
      accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
    ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
    INSERT INTO `{ns}_conduit`.conduit_token \
      (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
    ('PHID-USER-alice', 'api', 'TOKEN', NULL, 1, 1); \
    INSERT INTO `{ns}_dashboard`.dashboard_panel (id, phid, name, panelType, viewPolicy, \
      editPolicy, isArchived, properties, dateCreated, dateModified, authorPHID) VALUES \
    (1, 'PHID-DSHP-1', 'Notes', 'text', 'users', 'users', 0, '{\"text\":\"a/b\"}', 1, 1, \
      'PHID-USER-alice'), \
    (2, 'PHID-DSHP-2', 'Locked', 'text', 'users', 'admin', 0, '{}', 1, 1, 'PHID-USER-alice'); \
    INSERT INTO `{ns}_dashboard`.dashboard_portal (id, phid, name, status, viewPolicy, \
      editPolicy, properties, dateCreated, dateModified) VALUES \
    (1, 'PHID-PRTL-1', 'Home', 'active', 'users', 'users', '[]', 1, 1)";

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&[
        "dashboard",
        "user",
        "policy",
        "spaces",
        "conduit",
        "auth",
        "worker",
        "project",
        "herald",
    ])
    .await?;
    test_db.execute(&SEED.replace("TOKEN", TOKEN)).await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn call(context: &Context, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_dashboard::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": TOKEN});
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    call::dispatch(context, &registry, method, &http, Mode::Serve, NOW)
        .await
        .response
}

async fn rows(test_db: &TestDatabase, sql: &str) -> Vec<String> {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    phab_db::sqlx::query(&sql)
        .fetch_all(test_db.database().pool())
        .await
        .expect("rows")
        .iter()
        .map(|row| phab_db::rows::text(row, "value").expect("value"))
        .collect()
}

#[tokio::test]
async fn renames_panels_and_portals() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let rename = |id: Value, name: Value| json!({"objectIdentifier": id, "transactions": [{"type": "name", "value": name}]});
    let response = call(
        &context,
        "dashboard.panel.edit",
        rename(json!("PHID-DSHP-1"), json!("Daily notes")),
    )
    .await;
    let Response::Envelope(Envelope {
        result,
        error_code: None,
        ..
    }) = response
    else {
        panic!("expected an answer, got {response:?}");
    };
    assert_eq!(result["object"], json!({"id": 1, "phid": "PHID-DSHP-1"}));
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', name, properties, dateModified) AS value \
             FROM `{ns}_dashboard`.dashboard_panel WHERE id = 1",
        )
        .await,
        [format!("Daily notes | {{\"text\":\"a\\/b\"}} | {NOW}")]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', objectPHID, transactionType, oldValue, newValue) AS value \
             FROM `{ns}_dashboard`.dashboard_paneltransaction",
        )
        .await,
        ["PHID-DSHP-1 | dashpanel:name | \"Notes\" | \"Daily notes\""]
    );

    let response = call(&context, "portal.edit", rename(json!("1"), json!("Start"))).await;
    assert!(
        matches!(
            response,
            Response::Envelope(Envelope {
                error_code: None,
                ..
            })
        ),
        "{response:?}"
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', p.name, p.properties, x.transactionType, x.newValue) \
               AS value FROM `{ns}_dashboard`.dashboard_portal p \
             JOIN `{ns}_dashboard`.dashboard_portaltransaction x ON x.objectPHID = p.phid",
        )
        .await,
        ["Start | [] | name | \"Start\""]
    );
    let tasks = rows(
        &test_db,
        "SELECT objectPHID AS value FROM `{ns}_worker`.worker_activetask ORDER BY id",
    )
    .await;
    assert_eq!(tasks, ["PHID-DSHP-1", "PHID-PRTL-1"]);

    // Validation errors.
    assert_eq!(
        call(&context, "portal.edit", rename(json!(1), json!(""))).await,
        Response::Envelope(Envelope::error(
            "ERR-CONDUIT-CORE",
            "Validation errors:\n  - Portals must have a title."
        ))
    );
    // Left to PHP: a panel alice can't edit, edits without effect, other
    // types.
    for (method, params) in [
        ("dashboard.panel.edit", rename(json!(2), json!("X"))),
        ("portal.edit", rename(json!(1), json!("Start"))),
        (
            "dashboard.panel.edit",
            json!({"objectIdentifier": 1, "transactions": [{"type": "text", "value": "x"}]}),
        ),
    ] {
        let response = call(&context, method, params.clone()).await;
        assert!(
            matches!(response, Response::Fallback(_)),
            "{method} {params}: {response:?}"
        );
    }

    test_db.drop_databases().await;
}
