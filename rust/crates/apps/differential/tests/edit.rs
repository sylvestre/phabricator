//! `differential.revision.edit` comments against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-edededededededededededededed";

const DATABASES: &[&str] = &[
    "differential",
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
];

/// Mozilla's secure revision rules (H1, H2).
const RULES: &str = "INSERT INTO `{ns}_herald`.herald_rule (id, name, authorPHID, contentType, \
       mustMatchAll, configVersion, dateCreated, dateModified, repetitionPolicy, ruleType, phid, \
       isDisabled, triggerObjectPHID) VALUES \
     (1, 'Add Secure Revision Project Tag', 'PHID-USER-admin', 'differential', 1, 38, 1, 1, \
       'every', 'global', 'PHID-HRUL-one', 0, NULL), \
     (2, 'Secure Mail', 'PHID-USER-admin', 'differential', 0, 38, 1, 1, 'every', 'global', \
       'PHID-HRUL-two', 0, NULL); \
     INSERT INTO `{ns}_herald`.herald_condition (id, ruleID, fieldName, fieldCondition, value) \
       VALUES (1, 1, 'new-object', 'true', 'null'), \
     (2, 2, 'rule', 'conditions', '\"PHID-HRUL-one\"'), \
     (3, 2, 'projects', 'any', '[\"PHID-PROJ-secure\"]'); \
     INSERT INTO `{ns}_herald`.herald_action (id, ruleID, action, target) VALUES \
     (1, 1, 'projects.add', '[\"PHID-PROJ-secure\"]'), \
     (2, 2, 'email.must-encrypt', 'null'); \
     INSERT INTO `{ns}_herald`.herald_savedheader (phid, header) VALUES \
     ('PHID-DREV-1', '<1>, <2>')";

async fn edit(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_differential::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": TOKEN});
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    call::dispatch(
        context,
        &registry,
        "differential.revision.edit",
        &http,
        Mode::Serve,
        1_700_000_000,
    )
    .await
    .response
}

fn comment(text: &str) -> Value {
    json!({"objectIdentifier": "D1", "transactions": [{"type": "comment", "value": text}]})
}

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1); \
             INSERT INTO `{{ns}}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Open', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{{}}', \
               'k', 'users', 'users', NULL, '{{}}', 'PHID-DIFF-1'), \
             (2, 'Hidden', 'PHID-DREV-2', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, \
               '{{}}', 'k', 'admin', 'admin', NULL, '{{}}', 'PHID-DIFF-2'); \
             INSERT INTO `{{ns}}_differential`.differential_reviewer (revisionPHID, \
               reviewerPHID, reviewerStatus, dateCreated, dateModified, options) VALUES \
             ('PHID-DREV-1', 'PHID-USER-bob', 'added', 1, 1, '{{}}'), \
             ('PHID-DREV-1', 'PHID-USER-carol', 'resigned', 1, 1, '{{}}'); \
             INSERT INTO `{{ns}}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 41, 'PHID-PROJ-secure', 1, 0), \
             ('PHID-DREV-1', 21, 'PHID-USER-dave', 1, 0)"
        ))
        .await;
    test_db.execute(RULES).await;
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

async fn one_value(test_db: &TestDatabase, sql: &str) -> Value {
    let sql = sql.replace("{ns}", test_db.database().namespace());
    let row = phab_db::sqlx::query(&sql)
        .fetch_one(test_db.database().pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    json!(phab_db::rows::opt_text(&row, "value").expect("a value"))
}

#[tokio::test]
async fn writes_a_comment_like_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let Response::Envelope(Envelope {
        result,
        error_code: None,
        ..
    }) = edit(&context, comment("Looks good to me.")).await
    else {
        panic!("expected an answer");
    };
    assert_eq!(result["object"], json!({"id": 1, "phid": "PHID-DREV-1"}));
    let xaction = result["transactions"][0]["phid"]
        .as_str()
        .expect("a transaction")
        .to_owned();
    assert!(xaction.starts_with("PHID-XACT-DREV-"), "{xaction}");

    let value = |sql: &'static str| one_value(&test_db, sql);
    assert_eq!(
        value("SELECT CONCAT(transactionType, ' ', viewPolicy, ' ', editPolicy, ' ', metadata) AS value FROM `{ns}_differential`.differential_transaction").await
            .as_str()
            .map(|text| text.split("core.groupID").next().unwrap_or_default().to_owned()),
        Some("core:comment public PHID-USER-bob {\"".to_owned())
    );
    assert_eq!(
        value("SELECT content AS value FROM `{ns}_differential`.differential_transaction_comment")
            .await,
        "Looks good to me."
    );
    assert_eq!(
        value("SELECT CAST(dateModified AS CHAR) AS value FROM `{ns}_differential`.differential_revision WHERE id = 1").await,
        "1700000000"
    );
    // Bob reviews: his comment is on the active diff.
    assert_eq!(
        value("SELECT lastCommentDiffPHID AS value FROM `{ns}_differential`.differential_reviewer WHERE reviewerPHID = 'PHID-USER-bob'").await,
        "PHID-DIFF-1"
    );
    // H2 matched: the transcript says so, and the header keeps H1 too.
    let rules =
        value("SELECT CAST(ruleTranscripts AS CHAR) AS value FROM `{ns}_herald`.herald_transcript")
            .await;
    assert!(
        rules
            .as_str()
            .is_some_and(|text| text.contains("any-match")),
        "{rules}"
    );
    assert_eq!(
        value("SELECT header AS value FROM `{ns}_herald`.herald_savedheader").await,
        "<1>, <2>"
    );

    // The publish task, for PHP's daemons.
    assert_eq!(
        value("SELECT CONCAT(taskClass, ' ', priority, ' ', objectPHID) AS value FROM `{ns}_worker`.worker_activetask").await,
        "PhabricatorApplicationTransactionPublishWorker 1000 PHID-DREV-1"
    );
    let data = value("SELECT data AS value FROM `{ns}_worker`.worker_taskdata").await;
    let data: Value = serde_json::from_str(data.as_str().expect("data")).expect("JSON");
    assert_eq!(data["xactionPHIDs"], json!([xaction]));
    let state = &data["state"];
    assert_eq!(
        state["mailToPHIDs"],
        json!(["PHID-USER-alice", "PHID-USER-bob"])
    );
    assert_eq!(state["mailCCPHIDs"], json!(["PHID-USER-dave"]));
    assert_eq!(state["mailUnexpandablePHIDs"], json!(["PHID-USER-carol"]));
    assert_eq!(state["mustEncrypt"], json!(["PHID-HRUL-two"]));
    assert_eq!(state["heraldHeader"], "<1>, <2>");
    assert_eq!(
        state["feedRelatedPHIDs"],
        json!(["PHID-DREV-1", "PHID-USER-bob", "PHID-PROJ-secure"])
    );
    let herald = state["mailStamps"]
        .as_array()
        .expect("stamps")
        .iter()
        .find(|stamp| stamp["key"] == "herald")
        .expect("a herald stamp");
    assert_eq!(herald["value"], json!(["H1", "H2"]));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_the_rest_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    for params in [
        comment("Thanks @alice"),
        comment("See D2."),
        comment("   "),
        json!({"objectIdentifier": "D2", "transactions": [{"type": "comment", "value": "Hi"}]}),
        json!({"transactions": [{"type": "comment", "value": "New"}]}),
        json!({"objectIdentifier": "D1", "transactions": [{"type": "title", "value": "New"}]}),
    ] {
        assert!(
            matches!(edit(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }
    // Nothing was written.
    let sql = format!(
        "SELECT COUNT(*) AS n FROM `{}_differential`.differential_transaction",
        test_db.database().namespace()
    );
    let row = phab_db::sqlx::query(&sql)
        .fetch_one(test_db.database().pool())
        .await
        .expect("a count");
    assert_eq!(phab_db::rows::int(&row, "n").expect("n"), 0);

    test_db.drop_databases().await;
}

#[tokio::test]
async fn subscribes_who_comments() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // Bob neither reviews nor subscribes: commenting subscribes him.
    test_db
        .execute(
            "DELETE FROM `{ns}_differential`.differential_reviewer \
             WHERE reviewerPHID = 'PHID-USER-bob'",
        )
        .await;

    let Response::Envelope(Envelope {
        result,
        error_code: None,
        ..
    }) = edit(&context, comment("Looks good.")).await
    else {
        panic!("expected an answer");
    };
    let phids: Vec<&str> = result["transactions"]
        .as_array()
        .expect("transactions")
        .iter()
        .map(|xaction| xaction["phid"].as_str().expect("a PHID"))
        .collect();
    assert_eq!(phids.len(), 2);
    let value = |sql: &'static str| one_value(&test_db, sql);
    assert_eq!(
        value(
            "SELECT CONCAT(transactionType, ' ', oldValue, ' ', newValue) AS value FROM \
             `{ns}_differential`.differential_transaction ORDER BY id LIMIT 1"
        )
        .await,
        "core:subscribers [\"PHID-USER-dave\"] [\"PHID-USER-dave\",\"PHID-USER-bob\"]"
    );
    assert_eq!(
        value(
            "SELECT CONCAT(src, ' ', type) AS value FROM `{ns}_user`.edge WHERE \
             dst = 'PHID-DREV-1'"
        )
        .await,
        "PHID-USER-bob 22"
    );
    let data = value("SELECT data AS value FROM `{ns}_worker`.worker_taskdata").await;
    let data: Value = serde_json::from_str(data.as_str().expect("data")).expect("JSON");
    assert_eq!(data["xactionPHIDs"], json!(phids));
    // The newest subscriber first, as the edges are read.
    assert_eq!(
        data["state"]["mailCCPHIDs"],
        json!(["PHID-USER-bob", "PHID-USER-dave"])
    );

    test_db.drop_databases().await;
}

/// The status and the reviewer rows of D1.
async fn review_state(test_db: &TestDatabase) -> (String, Vec<(String, String)>) {
    let pool = test_db.database().pool();
    let namespace = test_db.database().namespace();
    let row = phab_db::sqlx::query(&format!(
        "SELECT status FROM `{namespace}_differential`.differential_revision WHERE id = 1"
    ))
    .fetch_one(pool)
    .await
    .expect("D1");
    let reviewers = phab_db::sqlx::query(&format!(
        "SELECT reviewerPHID, CONCAT(reviewerStatus, ' ', COALESCE(lastActionDiffPHID, '-'), \
         ' ', COALESCE(lastActorPHID, '-'), ' ', options) AS state FROM \
         `{namespace}_differential`.differential_reviewer ORDER BY id"
    ))
    .fetch_all(pool)
    .await
    .expect("reviewers")
    .iter()
    .map(|row| {
        (
            phab_db::rows::text(row, "reviewerPHID").expect("PHID"),
            phab_db::rows::text(row, "state").expect("state"),
        )
    })
    .collect();
    (
        phab_db::rows::text(&row, "status").expect("status"),
        reviewers,
    )
}

/// The transaction types written, in order.
async fn transaction_types(test_db: &TestDatabase) -> Vec<String> {
    phab_db::sqlx::query(&format!(
        "SELECT transactionType FROM `{}_differential`.differential_transaction ORDER BY id",
        test_db.database().namespace()
    ))
    .fetch_all(test_db.database().pool())
    .await
    .expect("transactions")
    .iter()
    .map(|row| phab_db::rows::text(row, "transactionType").expect("type"))
    .collect()
}

#[tokio::test]
async fn accepts_and_updates_the_status() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(
        &context,
        json!({"objectIdentifier": "D1", "transactions": [
            {"type": "comment", "value": "Ship it."},
            {"type": "accept", "value": true},
        ]}),
    )
    .await
    else {
        panic!("expected an answer");
    };
    // The action, then the comment, then the status the accept caused.
    assert_eq!(
        transaction_types(&test_db).await,
        [
            "differential.revision.accept",
            "core:comment",
            "differential.revision.status"
        ]
    );
    assert_eq!(
        review_state(&test_db).await,
        (
            "accepted".to_owned(),
            vec![
                (
                    "PHID-USER-bob".to_owned(),
                    "accepted PHID-DIFF-1 PHID-USER-bob {\"sticky\":null}".to_owned()
                ),
                ("PHID-USER-carol".to_owned(), "resigned - - {}".to_owned()),
            ]
        )
    );
    // Accepting again is refused, by PHP.
    assert!(matches!(
        edit(
            &context,
            json!({"objectIdentifier": "D1", "transactions": [{"type": "accept", "value": true}]})
        )
        .await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn comments_fix_a_stale_status() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // Bob accepted, but the revision still needs review.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_reviewer SET reviewerStatus = 'accepted' \
             WHERE reviewerPHID = 'PHID-USER-bob'",
        )
        .await;

    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(&context, comment("Looks good.")).await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        transaction_types(&test_db).await,
        ["core:comment", "differential.revision.status"]
    );
    assert_eq!(review_state(&test_db).await.0, "accepted");

    test_db.drop_databases().await;
}
