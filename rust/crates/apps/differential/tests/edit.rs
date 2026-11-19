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
        comment("See T2."),
        comment("   "),
        json!({"objectIdentifier": "D2", "transactions": [{"type": "comment", "value": "Hi"}]}),
        json!({"transactions": [{"type": "comment", "value": "New"}]}),
        json!({"objectIdentifier": "D1", "transactions": [{"type": "reviewers", "value": []}]}),
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

#[tokio::test]
async fn requests_changes() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(
        &context,
        json!({"objectIdentifier": "D1", "transactions": [{"type": "reject", "value": true}]}),
    )
    .await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        transaction_types(&test_db).await,
        [
            "differential.revision.reject",
            "differential.revision.status"
        ]
    );
    let (status, reviewers) = review_state(&test_db).await;
    assert_eq!(status, "needs-revision");
    assert_eq!(
        reviewers[0].1,
        "rejected PHID-DIFF-1 PHID-USER-bob {\"sticky\":null}"
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn resigns() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(
        &context,
        json!({"objectIdentifier": "D1", "transactions": [{"type": "resign", "value": true}]}),
    )
    .await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        transaction_types(&test_db).await,
        ["differential.revision.resign"]
    );
    let (status, reviewers) = review_state(&test_db).await;
    assert_eq!(status, "needs-review");
    assert_eq!(
        reviewers[0].1,
        "resigned PHID-DIFF-1 PHID-USER-bob {\"sticky\":null}"
    );
    // Resigning twice is refused, by PHP; Bob doesn't own D1 either.
    for kind in ["resign", "plan-changes", "abandon"] {
        let params =
            json!({"objectIdentifier": "D1", "transactions": [{"type": kind, "value": true}]});
        assert!(
            matches!(edit(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}

#[tokio::test]
async fn takes_author_actions() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // Bob owns D1 now.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_revision SET authorPHID = 'PHID-USER-bob' \
             WHERE id = 1; DELETE FROM `{ns}_differential`.differential_reviewer \
             WHERE reviewerPHID = 'PHID-USER-bob'",
        )
        .await;

    for (kind, status) in [
        ("plan-changes", "changes-planned"),
        ("abandon", "abandoned"),
    ] {
        let params =
            json!({"objectIdentifier": "D1", "transactions": [{"type": kind, "value": true}]});
        let Response::Envelope(Envelope {
            error_code: None, ..
        }) = edit(&context, params.clone()).await
        else {
            panic!("{params}: expected an answer");
        };
        assert_eq!(review_state(&test_db).await.0, status, "{params}");
    }
    assert_eq!(
        transaction_types(&test_db).await,
        [
            "differential.revision.plan",
            "differential.revision.abandon"
        ]
    );
    // An abandoned revision can't be abandoned again.
    assert!(matches!(
        edit(
            &context,
            json!({"objectIdentifier": "D1", "transactions": [{"type": "abandon", "value": true}]})
        )
        .await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn requests_review_again() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // Bob owns D1, which Dave accepted without sticking and Erin rejected.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_revision SET authorPHID = 'PHID-USER-bob', \
             status = 'needs-revision' WHERE id = 1; \
             DELETE FROM `{ns}_differential`.differential_reviewer; \
             INSERT INTO `{ns}_differential`.differential_reviewer (revisionPHID, reviewerPHID, \
               reviewerStatus, dateCreated, dateModified, options, lastActionDiffPHID) VALUES \
             ('PHID-DREV-1', 'PHID-USER-dave', 'accepted', 1, 1, '{\"sticky\":false}', \
               'PHID-DIFF-1'), \
             ('PHID-DREV-1', 'PHID-USER-erin', 'rejected', 1, 1, '{}', 'PHID-DIFF-1')",
        )
        .await;

    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(
        &context,
        json!({"objectIdentifier": "D1", "transactions": [{"type": "request-review", "value": true}]}),
    )
    .await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        transaction_types(&test_db).await,
        [
            "differential.revision.request",
            "differential.revision.void"
        ]
    );
    let value = |sql: &'static str| one_value(&test_db, sql);
    assert_eq!(
        value(
            "SELECT CONCAT(newValue, ' ', metadata) AS value FROM \
             `{ns}_differential`.differential_transaction WHERE transactionType = \
             'differential.revision.void'"
        )
        .await
        .as_str()
        .map(|text| text
            .split(",\"core.groupID")
            .next()
            .unwrap_or_default()
            .to_owned()),
        Some(
            "{\"3\":\"PHID-USER-dave\",\"4\":\"PHID-USER-erin\"} {\"void.force\":[\"accepted\"]"
                .to_owned()
        )
    );
    assert_eq!(
        value(
            "SELECT GROUP_CONCAT(voidedPHID) AS value FROM \
             `{ns}_differential`.differential_reviewer"
        )
        .await,
        "PHID-USER-bob,PHID-USER-bob"
    );
    // The voided reject still holds back an accept: "needs-review".
    assert_eq!(review_state(&test_db).await.0, "needs-review");

    test_db.drop_databases().await;
}

#[tokio::test]
async fn retitles() {
    let Some((test_db, context)) = setup().await else {
        return;
    };

    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(
        &context,
        json!({"objectIdentifier": "D1", "transactions": [{"type": "title", "value": "Better"}]}),
    )
    .await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        one_value(
            &test_db,
            "SELECT CONCAT(title, ' ', (SELECT CONCAT(oldValue, ' ', newValue) FROM \
             `{ns}_differential`.differential_transaction)) AS value FROM \
             `{ns}_differential`.differential_revision WHERE id = 1"
        )
        .await,
        "Better \"Open\" \"Better\""
    );
    // The same title again, and a title of a revision Bob can't edit.
    test_db
        .execute(
            "UPDATE `{ns}_differential`.differential_revision SET editPolicy = 'no-one' \
             WHERE id = 1",
        )
        .await;
    for title in ["Better", "Other"] {
        let params =
            json!({"objectIdentifier": "D1", "transactions": [{"type": "title", "value": title}]});
        assert!(
            matches!(edit(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}

#[tokio::test]
async fn subscribes_mentioned_users() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // Bob doesn't review D1: commenting subscribes him, and the mentions
    // subscribe Erin, not Dave (a subscriber) nor Alice (the author).
    test_db
        .execute(
            "DELETE FROM `{ns}_differential`.differential_reviewer \
             WHERE reviewerPHID = 'PHID-USER-bob'; \
             INSERT INTO `{ns}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (10, 'PHID-USER-erin', 'erin', 'Erin', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             (11, 'PHID-USER-dave', 'dave', 'Dave', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             (12, 'PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0)",
        )
        .await;

    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(&context, comment("Thanks @erin, @dave and @alice")).await
    else {
        panic!("expected an answer");
    };
    assert_eq!(
        one_value(
            &test_db,
            "SELECT newValue AS value FROM `{ns}_differential`.differential_transaction \
             WHERE transactionType = 'core:subscribers'"
        )
        .await,
        "[\"PHID-USER-dave\",\"PHID-USER-erin\",\"PHID-USER-bob\"]"
    );
    let data = one_value(
        &test_db,
        "SELECT data AS value FROM `{ns}_worker`.worker_taskdata",
    )
    .await;
    let data: Value = serde_json::from_str(data.as_str().expect("data")).expect("JSON");
    let mention = data["state"]["mailStamps"]
        .as_array()
        .expect("stamps")
        .iter()
        .find(|stamp| stamp["key"] == "mention")
        .expect("a mention stamp")["value"]
        .clone();
    // Newest users first, keyed by PHID.
    assert_eq!(
        mention,
        json!({
            "PHID-USER-alice": "PHID-USER-alice",
            "PHID-USER-dave": "PHID-USER-dave",
            "PHID-USER-erin": "PHID-USER-erin",
        })
    );
    assert_eq!(
        mention
            .as_object()
            .expect("a map")
            .keys()
            .collect::<Vec<_>>(),
        ["PHID-USER-alice", "PHID-USER-dave", "PHID-USER-erin"]
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn references_revisions() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (3, 'Other', 'PHID-DREV-3', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', \
               'k', 'users', 'users', NULL, '{}', 'PHID-DIFF-3')",
        )
        .await;

    // D2 is hidden and D1 can't mention itself: only D3 is mentioned.
    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(&context, comment("See D3, D2 and D1.")).await
    else {
        panic!("expected an answer");
    };
    let rows = phab_db::sqlx::query(&format!(
        "SELECT objectPHID, transactionType, newValue, metadata FROM \
         `{}_differential`.differential_transaction ORDER BY id",
        test_db.database().namespace()
    ))
    .fetch_all(test_db.database().pool())
    .await
    .expect("transactions");
    let found: Vec<(String, String, String)> = rows
        .iter()
        .map(|row| {
            let metadata = phab_db::rows::text(row, "metadata").expect("metadata");
            (
                phab_db::rows::text(row, "objectPHID").expect("object"),
                phab_db::rows::text(row, "transactionType").expect("type"),
                format!(
                    "{} {}",
                    phab_db::rows::text(row, "newValue").expect("new"),
                    metadata.split(",\"core").next().unwrap_or_default()
                ),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            (
                "PHID-DREV-1".to_owned(),
                "core:edge".to_owned(),
                "[\"PHID-DREV-3\"] {\"edge:type\":52".to_owned()
            ),
            (
                "PHID-DREV-1".to_owned(),
                "core:comment".to_owned(),
                "null {\"core.groupID\":\"".to_owned()
                    + &found[1].2["null {\"core.groupID\":\"".len()..]
            ),
            (
                "PHID-DREV-3".to_owned(),
                "core:edge".to_owned(),
                "[\"PHID-DREV-1\"] {\"edge:type\":51".to_owned()
            ),
        ]
    );
    // Both revisions get published, the mentioned one after.
    assert_eq!(
        one_value(
            &test_db,
            "SELECT GROUP_CONCAT(objectPHID ORDER BY id) AS value FROM \
             `{ns}_worker`.worker_activetask"
        )
        .await,
        "PHID-DREV-1,PHID-DREV-3"
    );
    assert_eq!(
        one_value(
            &test_db,
            "SELECT CAST(dateModified AS CHAR) AS value FROM \
             `{ns}_differential`.differential_revision WHERE id = 3"
        )
        .await,
        "1700000000"
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn edits_the_summary_and_test_plan() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (10, 'PHID-USER-erin', 'erin', 'Erin', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             UPDATE `{ns}_differential`.differential_revision SET summary = 'Ask @dave' \
             WHERE id = 1",
        )
        .await;

    // Dave was mentioned already; Erin is new.
    for (kind, text) in [("summary", "Ask @dave and @erin"), ("testPlan", "Ran it.")] {
        let params =
            json!({"objectIdentifier": "D1", "transactions": [{"type": kind, "value": text}]});
        let Response::Envelope(Envelope {
            error_code: None, ..
        }) = edit(&context, params.clone()).await
        else {
            panic!("{params}: expected an answer");
        };
    }
    assert_eq!(
        transaction_types(&test_db).await,
        [
            "differential.revision.summary",
            "core:subscribers",
            "differential.revision.testplan"
        ]
    );
    assert_eq!(
        one_value(
            &test_db,
            "SELECT CONCAT(summary, ' / ', testPlan) AS value FROM \
             `{ns}_differential`.differential_revision WHERE id = 1"
        )
        .await,
        "Ask @dave and @erin / Ran it."
    );
    assert_eq!(
        one_value(
            &test_db,
            "SELECT newValue AS value FROM `{ns}_differential`.differential_transaction \
             WHERE transactionType = 'core:subscribers'"
        )
        .await,
        "[\"PHID-USER-dave\",\"PHID-USER-erin\"]"
    );
    // A hidden revision is no dependency, nor a mention.
    let params = json!({"objectIdentifier": "D1", "transactions": [{"type": "summary", "value": "Depends on D2"}]});
    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(&context, params.clone()).await
    else {
        panic!("{params}: expected an answer");
    };
    assert_eq!(
        transaction_types(&test_db).await.last().map(String::as_str),
        Some("differential.revision.summary")
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn edits_reviewers() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (10, 'PHID-USER-erin', 'erin', 'Erin', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0)",
        )
        .await;
    let reviewers = |kind: &str, value: Value| json!({"objectIdentifier": "D1", "transactions": [{"type": kind, "value": value}]});
    for params in [
        reviewers("reviewers.add", json!(["blocking(PHID-USER-erin)"])),
        reviewers("reviewers.remove", json!(["PHID-USER-erin"])),
    ] {
        let Response::Envelope(Envelope {
            error_code: None, ..
        }) = edit(&context, params.clone()).await
        else {
            panic!("{params}: expected an answer");
        };
    }
    assert_eq!(
        one_value(
            &test_db,
            "SELECT GROUP_CONCAT(newValue ORDER BY id SEPARATOR ' / ') AS value FROM \
             `{ns}_differential`.differential_transaction \
             WHERE transactionType = 'differential.revision.reviewers'"
        )
        .await,
        "{\"PHID-USER-bob\":\"added\",\"PHID-USER-carol\":\"resigned\",\
         \"PHID-USER-erin\":\"blocking\"} / \
         {\"PHID-USER-bob\":\"added\",\"PHID-USER-carol\":\"resigned\"}"
    );
    let (_, rows) = review_state(&test_db).await;
    assert_eq!(rows.len(), 2);
    // Erin still hears of her removal.
    let data = one_value(
        &test_db,
        "SELECT data AS value FROM `{ns}_worker`.worker_taskdata ORDER BY id DESC LIMIT 1",
    )
    .await;
    let data: Value = serde_json::from_str(data.as_str().expect("data")).expect("JSON");
    assert_eq!(
        data["state"]["mailToPHIDs"],
        json!(["PHID-USER-alice", "PHID-USER-bob", "PHID-USER-erin"])
    );
    assert!(
        data["state"]["mailStamps"]
            .as_array()
            .expect("stamps")
            .contains(&json!({"type": "phid", "key": "removed", "value": ["PHID-USER-erin"]}))
    );
    // The author, unknown users and edits without effect are PHP's.
    for value in [
        json!(["PHID-USER-alice"]),
        json!(["PHID-USER-nobody"]),
        json!(["PHID-USER-bob"]),
        json!(["erin"]),
    ] {
        assert!(
            matches!(
                edit(&context, reviewers("reviewers.add", value.clone())).await,
                Response::Fallback(_)
            ),
            "{value}"
        );
    }

    test_db.drop_databases().await;
}

/// The edges of type `edge_type` in an application's database.
async fn edges_of(test_db: &TestDatabase, application: &str, edge_type: i64) -> Vec<String> {
    phab_db::sqlx::query(&format!(
        "SELECT CONCAT(src, ' ', dst, ' ', seq) AS edge FROM `{}_{application}`.edge \
         WHERE type = ? ORDER BY src, dst",
        test_db.database().namespace()
    ))
    .bind(edge_type)
    .fetch_all(test_db.database().pool())
    .await
    .expect("edges")
    .iter()
    .map(|row| phab_db::rows::text(row, "edge").expect("edge"))
    .collect()
}

#[tokio::test]
async fn edits_subscribers_and_tags() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user (id, phid, userName, realName, dateCreated, \
               dateModified, conduitCertificate, isSystemAgent, isDisabled, isAdmin, \
               isEmailVerified, isApproved, accountSecret, isEnrolledInMultiFactor, \
               isMailingList) VALUES \
             (10, 'PHID-USER-erin', 'erin', 'Erin', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{ns}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-1', 23, 'PHID-USER-erin', 1, 0); \
             INSERT INTO `{ns}_user`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-USER-erin', 24, 'PHID-DREV-1', 1, 0)",
        )
        .await;
    let params = |kind: &str, value: Value| json!({"objectIdentifier": "D1", "transactions": [{"type": kind, "value": value}]});
    // Erin unsubscribed once; Dave goes.
    for params in [
        params("subscribers.set", json!(["PHID-USER-erin"])),
        params("projects.remove", json!(["PHID-PROJ-secure"])),
    ] {
        let Response::Envelope(Envelope {
            error_code: None, ..
        }) = edit(&context, params.clone()).await
        else {
            panic!("{params}: expected an answer");
        };
    }
    assert_eq!(
        one_value(
            &test_db,
            "SELECT GROUP_CONCAT(CONCAT(transactionType, ' ', oldValue, ' ', newValue) \
             ORDER BY id SEPARATOR ' / ') AS value FROM \
             `{ns}_differential`.differential_transaction"
        )
        .await,
        "core:subscribers [\"PHID-USER-dave\"] [\"PHID-USER-erin\"] / \
         core:edge [\"PHID-PROJ-secure\"] []"
    );
    assert_eq!(
        edges_of(&test_db, "differential", 21).await,
        ["PHID-DREV-1 PHID-USER-erin 0"]
    );
    assert_eq!(
        edges_of(&test_db, "differential", 23).await,
        ["PHID-DREV-1 PHID-USER-dave 1"]
    );
    assert_eq!(
        edges_of(&test_db, "user", 22).await,
        ["PHID-USER-erin PHID-DREV-1 0"]
    );
    assert_eq!(
        edges_of(&test_db, "user", 24).await,
        ["PHID-USER-dave PHID-DREV-1 0"]
    );
    assert_eq!(
        edges_of(&test_db, "differential", 41).await,
        Vec::<String>::new()
    );
    // Without the secure tag, Herald no longer encrypts the mail.
    let data = one_value(
        &test_db,
        "SELECT data AS value FROM `{ns}_worker`.worker_taskdata ORDER BY id DESC LIMIT 1",
    )
    .await;
    let data: Value = serde_json::from_str(data.as_str().expect("data")).expect("JSON");
    assert_eq!(data["state"]["mustEncrypt"], json!([]));
    // Unknown subscribers and edits without effect are PHP's.
    for params in [
        params("subscribers.add", json!(["PHID-USER-nobody"])),
        params("subscribers.add", json!(["PHID-USER-erin"])),
        params("projects.add", json!(["PHID-PROJ-nothing"])),
    ] {
        assert!(
            matches!(edit(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}

#[tokio::test]
async fn declares_dependencies() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (3, 'Parent', 'PHID-DREV-3', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', \
               'k', 'users', 'users', NULL, '{}', 'PHID-DIFF-3')",
        )
        .await;
    let summary = |text: &str| json!({"objectIdentifier": "D1", "transactions": [{"type": "summary", "value": text}]});
    let Response::Envelope(Envelope {
        error_code: None, ..
    }) = edit(&context, summary("Depends on D3")).await
    else {
        panic!("expected an answer");
    };
    // The dependency, its inverse edit, and no mention.
    let edges = one_value(
        &test_db,
        "SELECT GROUP_CONCAT(CONCAT(src, ' ', type, ' ', dst) ORDER BY type SEPARATOR ' / ') \
         AS value FROM `{ns}_differential`.edge WHERE type IN (5, 6, 51, 52)",
    )
    .await;
    assert_eq!(
        edges,
        "PHID-DREV-1 5 PHID-DREV-3 / PHID-DREV-3 6 PHID-DREV-1"
    );
    let edge_transactions = one_value(
        &test_db,
        "SELECT GROUP_CONCAT(CONCAT(objectPHID, ' ', oldValue, ' ', newValue) ORDER BY id \
         SEPARATOR ' / ') AS value FROM `{ns}_differential`.differential_transaction \
         WHERE transactionType = 'core:edge'",
    )
    .await;
    assert_eq!(
        edge_transactions,
        "PHID-DREV-1 [] [\"PHID-DREV-3\"] / PHID-DREV-3 [] [\"PHID-DREV-1\"]"
    );
    // D3 depending on D1 already, D1 can't depend on it.
    test_db
        .execute(
            "DELETE FROM `{ns}_differential`.edge; \
             INSERT INTO `{ns}_differential`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-DREV-3', 5, 'PHID-DREV-1', 1, 0)",
        )
        .await;
    assert!(matches!(
        edit(&context, summary("Depends on D3, again")).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}
