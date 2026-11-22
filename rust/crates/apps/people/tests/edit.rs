//! `user.edit`, `user.enable` and `user.disable` against a test database.

// One database, many scenarios: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

/// Bob's token; bob is no administrator.
const BOB: &str = "api-usususususususususususususus";
/// The administrator's token.
const ADMIN: &str = "api-adadadadadadadadadadadadadad";
const NOW: i64 = 1_700_000_000;

const DATABASES: &[&str] = &[
    "user", "policy", "spaces", "conduit", "auth", "herald", "worker", "search", "project",
];

async fn call(context: &Context, token: &str, method: &str, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_people::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": token});
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

async fn answer(context: &Context, token: &str, method: &str, params: Value) -> Value {
    match call(context, token, method, params.clone()).await {
        Response::Envelope(Envelope {
            result,
            error_code: None,
            ..
        }) => result,
        other => panic!("{method} {params}: expected an answer, got {other:?}"),
    }
}

async fn error(context: &Context, token: &str, method: &str, params: Value) -> (String, String) {
    match call(context, token, method, params.clone()).await {
        Response::Envelope(Envelope {
            error_code: Some(code),
            error_info: Some(info),
            ..
        }) => (code, info),
        other => panic!("{method} {params}: expected an error, got {other:?}"),
    }
}

async fn fallback(context: &Context, token: &str, method: &str, params: Value) {
    let response = call(context, token, method, params.clone()).await;
    assert!(
        matches!(response, Response::Fallback(_)),
        "{method} {params}: expected PHP to answer, got {response:?}"
    );
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

/// The users, as `phid isDisabled dateModified`.
async fn users(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', phid, isDisabled, dateModified) AS line FROM `{ns}_user`.user \
         ORDER BY id",
    )
    .await
}

async fn profiles(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', userPHID, IF(title = '', '-', title), IF(blurb = '', '-', blurb), \
           IFNULL(profileImagePHID, '-'), icon, dateCreated, dateModified) AS line \
         FROM `{ns}_user`.user_profile ORDER BY id",
    )
    .await
}

async fn name_tokens(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', userID, token) AS line FROM `{ns}_user`.user_nametoken \
         ORDER BY userID, token",
    )
    .await
}

/// The transactions: object, type, old, new, metadata without the group,
/// and the columns every one shares.
async fn transactions(test_db: &TestDatabase) -> Vec<String> {
    lines(
        test_db,
        "SELECT CONCAT_WS(' ', objectPHID, transactionType, oldValue, newValue, \
           REPLACE(metadata, SUBSTRING(metadata, 18, 32), 'G'), authorPHID, viewPolicy, \
           editPolicy, IFNULL(commentPHID, '-'), commentVersion, contentSource, dateCreated, \
           dateModified, LEFT(phid, 15)) AS line \
         FROM `{ns}_user`.user_transaction ORDER BY id",
    )
    .await
}

/// The queued tasks, in order: class, priority, object and data.
async fn tasks(test_db: &TestDatabase) -> Vec<(String, Value)> {
    lines(
        test_db,
        "SELECT CONCAT(t.id, ' ', t.taskClass, ' ', t.priority, ' ', t.objectPHID, ' ', d.data) \
           AS line FROM `{ns}_worker`.worker_activetask t JOIN `{ns}_worker`.worker_taskdata d \
           ON d.id = t.dataID ORDER BY t.id",
    )
    .await
    .into_iter()
    .map(|line| {
        let (head, data) = line.split_at(line.find('{').expect("JSON"));
        (
            head.trim_end().to_owned(),
            serde_json::from_str(data).expect("JSON"),
        )
    })
    .collect()
}

/// The publish task PHP's user editor queues.
fn publish(user: &str, actor: &str, xactions: &[String]) -> Value {
    json!({
        "objectPHID": user,
        "actorPHID": actor,
        "xactionPHIDs": xactions,
        "state": {
            "parentMessageID": null,
            "isNewObject": false,
            "heraldEmailPHIDs": [],
            "heraldForcedEmailPHIDs": [],
            "heraldHeader": null,
            "mailToPHIDs": [],
            "mailCCPHIDs": [],
            "feedNotifyPHIDs": [],
            "feedRelatedPHIDs": [user, actor],
            "feedShouldPublish": true,
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

fn index(user: &str) -> Value {
    json!({"documentPHID": user, "parameters": [], "strict": false})
}

fn phids(result: &Value) -> Vec<String> {
    result["transactions"]
        .as_array()
        .expect("transactions")
        .iter()
        .map(|xaction| xaction["phid"].as_str().expect("a PHID").to_owned())
        .collect()
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (1, 'PHID-USER-bob', 'bob', 'Bob Smith', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             (2, 'PHID-USER-admin', 'admin', 'Admin', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0), \
             (3, 'PHID-USER-alice', 'alice', 'Alice (Ops)', 1, 1, 'c', 0, 0, 0, 1, 0, 's', 0, \
               0), \
             (4, 'PHID-USER-carol', 'carol', 'Carol', 1, 1, 'c', 0, 1, 0, 1, 1, 's', 0, 0), \
             (5, 'PHID-USER-zoe', 'zoe', 'Zoé', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_user`.user_profile (userPHID, title, blurb, profileImagePHID, \
               icon, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'Builder', 'Builds', NULL, 'engineering', 5, 6); \
             INSERT INTO `{{ns}}_user`.user_nametoken (userID, token) VALUES (1, 'stale'); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{BOB}', NULL, 1, 1), \
             ('PHID-USER-admin', 'api', '{ADMIN}', NULL, 1, 1)"
        ))
        .await;
    let mut config = Config::default();
    if let Value::Object(map) = json!({"phabricator.base-uri": "http://phabricator.test/"}) {
        config.push_layer("test", map);
    }
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn edits_users_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let edit = |identifier: Value, transactions: Value| json!({"objectIdentifier": identifier, "transactions": transactions});
    let disable = |value: bool| json!([{"type": "disabled", "value": value}]);

    // Bob may not disable users: PHP raises a policy exception.
    fallback(
        &context,
        BOB,
        "user.edit",
        edit(json!("PHID-USER-alice"), disable(true)),
    )
    .await;
    // Nor edit someone else without transactions (CAN_EDIT).
    fallback(&context, BOB, "user.edit", edit(json!(3), json!([]))).await;
    // Left to PHP: custom fields, usernames, creating users, odd values.
    for params in [
        edit(json!(1), json!([{"type": "title", "value": "Boss"}])),
        edit(json!("@alice"), disable(true)),
        json!({"transactions": disable(true)}),
        edit(json!(1), json!([{"type": "disabled", "value": "true"}])),
        edit(
            json!(1),
            json!([{"type": "disabled", "value": true}, {"type": "disabled", "value": false}]),
        ),
        edit(json!("PHID-USER-nobody"), disable(true)),
    ] {
        fallback(&context, ADMIN, "user.edit", params).await;
    }
    // Approving sends mail, rejecting is left to PHP too.
    fallback(
        &context,
        ADMIN,
        "user.edit",
        edit(json!(3), json!([{"type": "approved", "value": true}])),
    )
    .await;
    // Names PHP lowercases with mbstring.
    fallback(&context, ADMIN, "user.edit", edit(json!(5), disable(true))).await;
    assert_eq!(tasks(&test_db).await, Vec::new());

    // Only administrators approve users.
    assert_eq!(
        error(
            &context,
            BOB,
            "user.edit",
            edit(json!("3"), json!([{"type": "approved", "value": true}])),
        )
        .await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Validation errors:\n  - You must be an administrator to approve users.".to_owned()
        )
    );
    // No one disables themselves.
    assert_eq!(
        error(&context, ADMIN, "user.edit", edit(json!(2), disable(true))).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Validation errors:\n  - You can not enable or disable your own account.".to_owned()
        )
    );
    assert_eq!(tasks(&test_db).await, Vec::new());

    // The admin disables Alice: her profile is created, her name tokens
    // written, the user indexed and the edit published.
    let result = answer(
        &context,
        ADMIN,
        "user.edit",
        edit(json!("PHID-USER-alice"), disable(true)),
    )
    .await;
    let alice_xactions = phids(&result);
    assert_eq!(
        result,
        json!({
            "object": {"id": 3, "phid": "PHID-USER-alice"},
            "transactions": [{"phid": alice_xactions[0]}],
        })
    );
    assert_eq!(
        users(&test_db).await,
        vec![
            "PHID-USER-bob 0 1",
            "PHID-USER-admin 0 1",
            "PHID-USER-alice 1 1700000000",
            "PHID-USER-carol 1 1",
            "PHID-USER-zoe 0 1",
        ]
    );
    assert_eq!(
        profiles(&test_db).await,
        vec![
            "PHID-USER-bob Builder Builds - engineering 5 6",
            "PHID-USER-alice - - - person 1700000000 1700000000",
        ]
    );
    assert_eq!(
        name_tokens(&test_db).await,
        vec!["1 stale", "3 alice", "3 ops"]
    );
    let source = r#"{"source":"conduit","params":[]}"#;
    assert_eq!(
        transactions(&test_db).await,
        vec![format!(
            "PHID-USER-alice user.disable false true {{\"core.groupID\":\"G\"}} PHID-USER-admin \
             public PHID-USER-admin - 0 {source} 1700000000 1700000000 PHID-XACT-USER-"
        )]
    );
    assert_eq!(
        tasks(&test_db).await,
        vec![
            (
                "1 PhabricatorSearchWorker 3500 PHID-USER-alice".to_owned(),
                index("PHID-USER-alice")
            ),
            (
                "2 PhabricatorApplicationTransactionPublishWorker 1000 PHID-USER-alice".to_owned(),
                publish("PHID-USER-alice", "PHID-USER-admin", &alice_xactions)
            ),
        ]
    );

    // Bob "enables" Carol... who is disabled: he may not. Enabling Bob,
    // who is enabled, has no effect, which anyone may do: the user is
    // saved all the same.
    fallback(
        &context,
        BOB,
        "user.edit",
        edit(json!("PHID-USER-carol"), disable(false)),
    )
    .await;
    let result = answer(
        &context,
        BOB,
        "user.edit",
        edit(
            json!("1"),
            json!([{"type": "disabled", "value": false}, {"type": "approved", "value": true}]),
        ),
    )
    .await;
    assert_eq!(
        result,
        json!({"object": {"id": 1, "phid": "PHID-USER-bob"}, "transactions": []})
    );
    assert_eq!(users(&test_db).await[0], "PHID-USER-bob 0 1700000000");
    assert_eq!(
        profiles(&test_db).await[0],
        "PHID-USER-bob Builder Builds - engineering 5 1700000000"
    );
    assert_eq!(
        name_tokens(&test_db).await,
        vec!["1 bob", "1 smith", "3 alice", "3 ops"]
    );
    assert_eq!(transactions(&test_db).await.len(), 1);
    let queued = tasks(&test_db).await;
    assert_eq!(queued.len(), 4);
    assert_eq!(queued[3].1, publish("PHID-USER-bob", "PHID-USER-bob", &[]));
    // Bob may edit himself without transactions.
    answer(&context, BOB, "user.edit", edit(json!(1), json!([]))).await;
    assert_eq!(tasks(&test_db).await.len(), 6);
}

#[tokio::test]
async fn enables_and_disables_users_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let phids = |phids: &[&str]| json!({"phids": phids});

    assert_eq!(
        error(&context, BOB, "user.disable", phids(&["PHID-USER-alice"])).await,
        (
            "ERR-PERMISSIONS".to_owned(),
            "Only admins can call this method.".to_owned()
        )
    );
    for list in [
        &["PHID-USER-nobody"][..],
        &["PHID-USER-alice", "PHID-USER-alice"],
        &["PHID-USER-alice", "PHID-USER-nobody"],
    ] {
        assert_eq!(
            error(&context, ADMIN, "user.disable", phids(list)).await,
            (
                "ERR-BAD-PHID".to_owned(),
                "Non existent user PHID.".to_owned()
            )
        );
    }
    fallback(&context, ADMIN, "user.disable", json!({"phids": []})).await;
    fallback(&context, ADMIN, "user.disable", json!({})).await;
    // The admin can't disable themself; PHP stops at the first error.
    assert_eq!(
        error(&context, ADMIN, "user.disable", phids(&["PHID-USER-admin"])).await,
        (
            "ERR-CONDUIT-CORE".to_owned(),
            "Validation errors:\n  - You can not enable or disable your own account.".to_owned()
        )
    );
    // After an edit, PHP would have written the first user.
    fallback(
        &context,
        ADMIN,
        "user.disable",
        phids(&["PHID-USER-alice", "PHID-USER-admin"]),
    )
    .await;
    assert_eq!(tasks(&test_db).await, Vec::new());

    // Carol is already disabled: no transaction, but saved and published.
    let result = answer(
        &context,
        ADMIN,
        "user.disable",
        phids(&["PHID-USER-alice", "PHID-USER-carol"]),
    )
    .await;
    assert_eq!(result, Value::Null);
    assert_eq!(
        users(&test_db).await,
        vec![
            "PHID-USER-bob 0 1",
            "PHID-USER-admin 0 1",
            "PHID-USER-alice 1 1700000000",
            "PHID-USER-carol 1 1700000000",
            "PHID-USER-zoe 0 1",
        ]
    );
    let xactions = transactions(&test_db).await;
    assert_eq!(xactions.len(), 1);
    assert!(xactions[0].starts_with("PHID-USER-alice user.disable false true"));
    let heads: Vec<String> = tasks(&test_db)
        .await
        .into_iter()
        .map(|(head, _)| head)
        .collect();
    assert_eq!(
        heads,
        vec![
            "1 PhabricatorSearchWorker 3500 PHID-USER-alice",
            "2 PhabricatorApplicationTransactionPublishWorker 1000 PHID-USER-alice",
            "3 PhabricatorSearchWorker 3500 PHID-USER-carol",
            "4 PhabricatorApplicationTransactionPublishWorker 1000 PHID-USER-carol",
        ]
    );

    // And back.
    assert_eq!(
        answer(&context, ADMIN, "user.enable", phids(&["PHID-USER-carol"])).await,
        Value::Null
    );
    assert_eq!(users(&test_db).await[3], "PHID-USER-carol 0 1700000000");
    let xactions = transactions(&test_db).await;
    assert!(xactions[1].starts_with("PHID-USER-carol user.disable true false"));
}
