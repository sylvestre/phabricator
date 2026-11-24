//! `diffusion.commit.edit` against a test database.
//!
//! Alice, who calls, edits the commits of the Git repository 1: commit 1
//! is Bob's, commit 2 has no known author, commit 3 is on no permanent
//! ref (it doesn't publish). Commit 4 is in the Mercurial repository 2.
//! Alice is a member of the project "team".

// One fixture, many scenarios: creating schemas is slow.
#![allow(clippy::too_many_lines)]

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-cmcmcmcmcmcmcmcmcmcmcmcmcmcm";
const NOW: i64 = 1_700_000_000;
const ALICE: &str = "PHID-USER-alice";
const BOB: &str = "PHID-USER-bob";
const TEAM: &str = "PHID-PROJ-team";

const SEED: &str = "\
    INSERT INTO `{ns}_user`.user (phid, userName, realName, dateCreated, dateModified, \
      conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
      accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
    ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
    ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
    INSERT INTO `{ns}_conduit`.conduit_token \
      (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
    ('PHID-USER-alice', 'api', 'TOKEN', NULL, 1, 1); \
    INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, repositorySlug, \
      versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
      pushPolicy, localPath, almanacServicePHID) VALUES \
    (1, 'PHID-REPO-one', 'One', 'ONE', NULL, 'git', '{}', 1, 1, 'users', 'users', 'users', \
      '/var/repo/1/', NULL), \
    (2, 'PHID-REPO-two', 'Two', 'TWO', NULL, 'hg', '{}', 1, 1, 'users', 'users', 'users', \
      '/var/repo/2/', NULL); \
    INSERT INTO `{ns}_repository`.repository_commit (id, repositoryID, phid, \
      commitIdentifier, epoch, authorPHID, auditStatus, summary, importStatus) VALUES \
    (1, 1, 'PHID-CMIT-one', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 10, 'PHID-USER-bob', \
      'none', 'One', 1035), \
    (2, 1, 'PHID-CMIT-two', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 20, NULL, 'none', \
      'Two', 1035), \
    (3, 1, 'PHID-CMIT-three', 'cccccccccccccccccccccccccccccccccccccccc', 30, NULL, 'none', \
      'Three', 11), \
    (4, 2, 'PHID-CMIT-four', 'dddddddddddddddddddddddddddddddddddddddd', 40, NULL, 'none', \
      'Four', 1035); \
    INSERT INTO `{ns}_repository`.repository_commitdata (commitID, authorName, \
      commitMessage, commitDetails) VALUES \
    (1, 'bob', 'One', '{\"authorPHID\":\"PHID-USER-bob\"}'), (2, 'x', 'Two', '[]'), \
    (3, 'x', 'Three', '[]'), (4, 'x', 'Four', '[]'); \
    INSERT INTO `{ns}_herald`.herald_savedheader (phid, header) VALUES \
    ('PHID-CMIT-one', 'none'); \
    INSERT INTO `{ns}_project`.project (id, name, phid, authorPHID, dateCreated, \
      dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
      hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
      projectPathKey, properties, subtype, parentProjectPHID, milestoneNumber, \
      primarySlug) VALUES \
    (1, 'Team', 'PHID-PROJ-team', 'PHID-USER-x', 1, 1, '0', 'users', 'users', 'users', \
      'fa-x', 'blue', 'k', 0, 0, 0, 'p1', 0, 'k001', '{}', 'default', NULL, NULL, 'team'); \
    INSERT INTO `{ns}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
    ('PHID-PROJ-team', 13, 'PHID-USER-alice', 1, 0), \
    ('PHID-PROJ-team', 60, 'PHID-USER-alice', 1, 0)";

async fn setup() -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&[
        "repository",
        "user",
        "policy",
        "spaces",
        "conduit",
        "auth",
        "worker",
        "project",
        "herald",
        "audit",
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

async fn call(context: &Context, id: Value, transactions: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_diffusion::register(&mut registry);
    let params = json!({
        "objectIdentifier": id,
        "transactions": transactions,
        "__conduit__": {"token": TOKEN},
    });
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
        "diffusion.commit.edit",
        &http,
        Mode::Serve,
        NOW,
    )
    .await
    .response
}

fn result(response: Response) -> Value {
    let Response::Envelope(Envelope {
        result,
        error_code: None,
        ..
    }) = response
    else {
        panic!("expected an answer, got {response:?}");
    };
    result
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

/// The transactions of a commit, oldest first.
async fn transactions(test_db: &TestDatabase, commit: &str) -> Vec<String> {
    rows(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' | ', transactionType, oldValue, newValue) AS value \
             FROM `{{ns}}_audit`.audit_transaction WHERE objectPHID = '{commit}' ORDER BY id"
        ),
    )
    .await
}

/// The audit requests of a commit and its status.
async fn audits(test_db: &TestDatabase, commit: &str) -> (Vec<String>, String) {
    let audits = rows(
        test_db,
        &format!(
            "SELECT CONCAT_WS(' ', id, auditorPHID, auditStatus, auditReasons) AS value \
             FROM `{{ns}}_repository`.repository_auditrequest WHERE commitPHID = '{commit}' \
             ORDER BY id"
        ),
    )
    .await;
    let status = rows(
        test_db,
        &format!(
            "SELECT auditStatus AS value FROM `{{ns}}_repository`.repository_commit \
             WHERE phid = '{commit}'"
        ),
    )
    .await;
    (audits, status[0].clone())
}

/// The data of the newest publish task.
async fn last_task(test_db: &TestDatabase) -> Value {
    let data = rows(
        test_db,
        "SELECT d.data AS value FROM `{ns}_worker`.worker_activetask t \
         JOIN `{ns}_worker`.worker_taskdata d ON d.id = t.dataID \
         ORDER BY t.id DESC LIMIT 1",
    )
    .await;
    serde_json::from_str(&data[0]).expect("task data")
}

#[tokio::test]
async fn comments_on_commits() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let answer = result(
        call(
            &context,
            json!(2),
            json!([{"type": "comment", "value": "Looks right."}]),
        )
        .await,
    );
    assert_eq!(answer["object"], json!({"id": 2, "phid": "PHID-CMIT-two"}));
    assert_eq!(answer["transactions"].as_array().map(Vec::len), Some(2));
    // The implicit CC comes first, the comment last.
    assert_eq!(
        transactions(&test_db, "PHID-CMIT-two").await,
        [
            format!("core:subscribers | [] | [\"{ALICE}\"]"),
            "core:comment | null | null".to_owned(),
        ]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' | ', content, authorPHID, editPolicy, commentVersion, \
             IFNULL(commitPHID, 'none'), attributes) AS value \
             FROM `{ns}_audit`.audit_transaction_comment",
        )
        .await,
        [format!("Looks right. | {ALICE} | {ALICE} | 1 | none | []")]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' ', src, type, dst) AS value FROM `{ns}_repository`.edge",
        )
        .await,
        [format!("PHID-CMIT-two 21 {ALICE}")]
    );
    let task = last_task(&test_db).await;
    assert_eq!(task["objectPHID"], "PHID-CMIT-two");
    assert_eq!(task["actorPHID"], ALICE);
    let state = &task["state"];
    assert_eq!(state["mailShouldSend"], true);
    assert_eq!(state["mailToPHIDs"], json!([ALICE]));
    assert_eq!(state["mailCCPHIDs"], json!([ALICE]));
    assert_eq!(state["feedNotifyPHIDs"], json!([ALICE]));
    assert_eq!(state["heraldHeader"], Value::Null);
    assert_eq!(state["custom"]["auditorPHIDs"], json!([]));
    assert_eq!(
        state["mailStamps"][1]["value"],
        format!("rONE{}", "b".repeat(40))
    );
    // The commit's status doesn't change: no state transaction.
    assert_eq!(audits(&test_db, "PHID-CMIT-two").await.1, "none");

    // Bob's commit: he gets the mail as its author; the saved header is
    // kept.
    result(
        call(
            &context,
            json!("PHID-CMIT-one"),
            json!([{"type": "comment", "value": "Thanks."}]),
        )
        .await,
    );
    let state = &last_task(&test_db).await["state"];
    assert_eq!(state["mailToPHIDs"], json!([BOB, ALICE]));
    assert_eq!(state["heraldHeader"], "none");

    // A commit on no permanent ref publishes nothing.
    result(
        call(
            &context,
            json!(3),
            json!([{"type": "comment", "value": "Unpublished."}]),
        )
        .await,
    );
    let state = &last_task(&test_db).await["state"];
    assert_eq!(state["mailShouldSend"], false);
    assert_eq!(state["feedShouldPublish"], false);
    assert_eq!(state["mailStamps"], json!([]));
}

#[tokio::test]
async fn audits_commits() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    // Bob audits commit 2, which now needs an audit.
    let answer = result(
        call(
            &context,
            json!("PHID-CMIT-two"),
            json!([{"type": "auditors.add", "value": [BOB, BOB]}]),
        )
        .await,
    );
    assert_eq!(answer["transactions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        audits(&test_db, "PHID-CMIT-two").await,
        (
            vec![format!("1 {BOB} requested []")],
            "needs-audit".to_owned()
        )
    );
    assert_eq!(
        transactions(&test_db, "PHID-CMIT-two").await,
        [
            format!("diffusion.commit.auditors | [] | {{\"{BOB}\":\"requested\"}}"),
            "diffusion.commit.state | \"none\" | \"needs-audit\"".to_owned(),
        ]
    );
    let state = &last_task(&test_db).await["state"];
    assert_eq!(state["mailToPHIDs"], json!([BOB, ALICE]));
    assert_eq!(state["custom"]["auditorPHIDs"], json!({"1": BOB}));

    // Alice accepts it: Bob still has to, so it's partially audited, which
    // gets no state transaction.
    result(
        call(
            &context,
            json!(2),
            json!([{"type": "accept", "value": true}, {"type": "comment", "value": "Fine."}]),
        )
        .await,
    );
    assert_eq!(
        audits(&test_db, "PHID-CMIT-two").await,
        (
            vec![
                format!("1 {BOB} requested []"),
                format!("2 {ALICE} accepted []")
            ],
            "partially-audited".to_owned()
        )
    );
    let written = transactions(&test_db, "PHID-CMIT-two").await;
    assert_eq!(
        written[2..],
        [
            format!("core:subscribers | [] | [\"{ALICE}\"]"),
            "diffusion.commit.accept | false | true".to_owned(),
            "core:comment | null | null".to_owned(),
        ]
    );
    // Accepting again is refused by PHP.
    assert!(matches!(
        call(
            &context,
            json!(2),
            json!([{"type": "accept", "value": true}])
        )
        .await,
        Response::Fallback(_)
    ));

    // Alice raises a concern with Bob's commit, as a member of the team
    // auditing it too.
    result(
        call(
            &context,
            json!(1),
            json!([{"type": "auditors.add", "value": [TEAM]}]),
        )
        .await,
    );
    result(
        call(
            &context,
            json!(1),
            json!([{"type": "concern", "value": true}]),
        )
        .await,
    );
    assert_eq!(
        audits(&test_db, "PHID-CMIT-one").await,
        (
            vec![
                format!("3 {TEAM} concerned []"),
                format!("4 {ALICE} concerned []")
            ],
            "concern-raised".to_owned()
        )
    );
    assert_eq!(
        transactions(&test_db, "PHID-CMIT-one").await[2..],
        [
            "diffusion.commit.concern | false | true".to_owned(),
            "diffusion.commit.state | \"needs-audit\" | \"concern-raised\"".to_owned(),
        ]
    );

    // Resigning only affects oneself.
    result(
        call(
            &context,
            json!(1),
            json!([{"type": "resign", "value": true}]),
        )
        .await,
    );
    assert_eq!(
        audits(&test_db, "PHID-CMIT-one").await.0,
        [
            format!("3 {TEAM} concerned []"),
            format!("4 {ALICE} resigned []")
        ]
    );
    let state = &last_task(&test_db).await["state"];
    assert_eq!(state["mailUnexpandablePHIDs"], json!([ALICE]));

    // Removing the team: no audit is left.
    result(
        call(
            &context,
            json!(1),
            json!([{"type": "auditors.remove", "value": [TEAM]}]),
        )
        .await,
    );
    assert_eq!(
        audits(&test_db, "PHID-CMIT-one").await,
        (vec![format!("4 {ALICE} resigned []")], "none".to_owned())
    );
}

#[tokio::test]
async fn edits_subscribers_and_projects() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    result(
        call(
            &context,
            json!(1),
            json!([
                {"type": "projects.add", "value": [TEAM]},
                {"type": "subscribers.add", "value": [BOB]},
            ]),
        )
        .await,
    );
    assert_eq!(
        transactions(&test_db, "PHID-CMIT-one").await,
        [
            format!("core:edge | [] | [\"{TEAM}\"]"),
            format!("core:subscribers | [] | [\"{BOB}\"]"),
        ]
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' ', src, type, dst) AS value FROM `{ns}_project`.edge \
             WHERE type = 42",
        )
        .await,
        [format!("{TEAM} 42 PHID-CMIT-one")]
    );
    let state = &last_task(&test_db).await["state"];
    assert_eq!(
        state["feedRelatedPHIDs"],
        json!(["PHID-CMIT-one", ALICE, TEAM])
    );

    // Bob, the author, gets the mail anyway; on commit 2, which has no
    // author, unsubscribing him leaves an "unsubscribed" edge, and he still
    // gets the mail.
    result(
        call(
            &context,
            json!(1),
            json!([{"type": "subscribers.remove", "value": [BOB]}]),
        )
        .await,
    );
    let state = &last_task(&test_db).await["state"];
    assert_eq!(state["mailToPHIDs"], json!([BOB, ALICE]));
    assert_eq!(state["mailCCPHIDs"], json!([]));
    result(
        call(
            &context,
            json!(2),
            json!([{"type": "subscribers.add", "value": [BOB]}]),
        )
        .await,
    );
    result(
        call(
            &context,
            json!(2),
            json!([{"type": "subscribers.remove", "value": [BOB]}]),
        )
        .await,
    );
    assert_eq!(
        rows(
            &test_db,
            "SELECT CONCAT_WS(' ', src, type, dst) AS value FROM `{ns}_repository`.edge \
             WHERE type IN (21, 23) ORDER BY src, type",
        )
        .await,
        [
            format!("PHID-CMIT-one 23 {BOB}"),
            format!("PHID-CMIT-two 23 {BOB}")
        ]
    );
    let state = &last_task(&test_db).await["state"];
    assert_eq!(state["mailToPHIDs"], json!([ALICE]));
    assert_eq!(state["mailCCPHIDs"], json!([BOB]));
    assert_eq!(state["mailStamps"][11]["value"], json!([BOB]));
}

#[tokio::test]
async fn leaves_the_rest_to_php() {
    let Some((test_db, context)) = setup().await else {
        return;
    };
    let fallback = |response: Response| matches!(response, Response::Fallback(_));
    for (id, transactions) in [
        // Mercurial.
        (json!(4), json!([{"type": "comment", "value": "x"}])),
        // A monogram.
        (
            json!("rONEaaaa"),
            json!([{"type": "comment", "value": "x"}]),
        ),
        // Bob is the author.
        (json!(1), json!([{"type": "auditors.add", "value": [BOB]}])),
        // Alice can't request verification of Bob's commit.
        (
            json!(1),
            json!([{"type": "request-verification", "value": true}]),
        ),
        // She is no auditor.
        (json!(2), json!([{"type": "resign", "value": true}])),
        // Mentions.
        (
            json!(2),
            json!([{"type": "comment", "value": "@bob, see D12."}]),
        ),
        // Auditors with an action.
        (
            json!(2),
            json!([
                {"type": "auditors.add", "value": [BOB]},
                {"type": "accept", "value": true},
            ]),
        ),
        // No effect.
        (
            json!(2),
            json!([{"type": "auditors.remove", "value": [BOB]}]),
        ),
        // Inline comments.
        (json!(2), json!([{"type": "inline", "value": {}}])),
    ] {
        assert!(
            fallback(call(&context, id.clone(), transactions.clone()).await),
            "{id} {transactions}"
        );
    }
    assert!(
        rows(
            &test_db,
            "SELECT phid AS value FROM `{ns}_audit`.audit_transaction",
        )
        .await
        .is_empty()
    );
}
