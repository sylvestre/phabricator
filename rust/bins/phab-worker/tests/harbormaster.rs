//! `HarbormasterBuildWorker` and `HarbormasterTargetWorker` against a
//! throwaway `MySQL` database: the rows they leave, as
//! `HarbormasterBuildEngine` and `HarbormasterTargetWorker::doWork()`
//! write them (derived from the PHP code, see each test), and the tasks
//! left to PHP untouched.
//!
//! Revision 1 has diff 1, whose buildable B1 is building; build 1 runs the
//! "Try" plan, whose "report" step depends on its "run" step. Both steps
//! make HTTP requests to `ci.example.test`, which doesn't resolve.

// The checks of one scenario stay together.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use phab_app_harbormaster::engine::{HttpClient, HttpFuture, HttpRequest, HttpResponse};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_db::worker::{Task, schedule_task};
use phab_db::{Database, rows};
use phab_worker::handlers::harbormaster::{BuildWorker, TargetWorker};
use phab_worker::{Executed, LeaseOwner, Registry, Worker};
use serde_json::{Map, Value, json};

const DATABASES: &[&str] = &[
    "worker",
    "harbormaster",
    "differential",
    "repository",
    "herald",
];

type Row = Map<String, Value>;

struct Fixture {
    test_db: TestDatabase,
}

async fn fixture() -> Option<Fixture> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(
            "INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Ships', 'PHID-DREV-1', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', \
               'k', 'users', 'users', NULL, '{}', 'PHID-DIFF-1'); \
             INSERT INTO `{ns}_differential`.differential_diff (id, phid, revisionID, \
               authorPHID, lintStatus, unitStatus, lineCount, dateCreated, dateModified, \
               viewPolicy) VALUES \
             (1, 'PHID-DIFF-1', 1, 'PHID-USER-alice', 0, 0, 1, 1, 1, 'users'); \
             INSERT INTO `{ns}_harbormaster`.harbormaster_buildable (id, phid, buildablePHID, \
               containerPHID, buildableStatus, dateCreated, dateModified, isManualBuildable) \
             VALUES (1, 'PHID-HMBB-1', 'PHID-DIFF-1', 'PHID-DREV-1', 'building', 10, 11, 0); \
             INSERT INTO `{ns}_harbormaster`.harbormaster_buildplan (id, phid, name, planStatus, \
               dateCreated, dateModified, planAutoKey, viewPolicy, editPolicy, properties) VALUES \
             (1, 'PHID-HMCP-1', 'Try', 'active', 100, 101, NULL, 'users', 'admin', '[]'); \
             INSERT INTO `{ns}_harbormaster`.harbormaster_buildstep (id, phid, buildPlanPHID, \
               className, details, dateCreated, dateModified, sequence, name, description, \
               stepAutoKey) VALUES \
             (1, 'PHID-HMCS-run', 'PHID-HMCP-1', \
               'HarbormasterHTTPRequestBuildStepImplementation', \
               '{\"uri\":\"https:\\\\/\\\\/ci.example.test\\\\/run?b=${build.id}\",\"method\":\"POST\"}', \
               1, 1, 0, 'Run', '', NULL), \
             (2, 'PHID-HMCS-report', 'PHID-HMCP-1', \
               'HarbormasterHTTPRequestBuildStepImplementation', \
               '{\"uri\":\"https:\\\\/\\\\/ci.example.test\\\\/report\",\"dependsOn\":[\"PHID-HMCS-run\"]}', \
               1, 1, 0, 'Report', '', NULL); \
             INSERT INTO `{ns}_harbormaster`.harbormaster_build (id, phid, buildablePHID, \
               buildPlanPHID, buildStatus, dateCreated, dateModified, buildGeneration, \
               planAutoKey, buildParameters, initiatorPHID) VALUES \
             (1, 'PHID-HMBD-1', 'PHID-HMBB-1', 'PHID-HMCP-1', 'building', 1000, 1001, 0, NULL, \
               '[]', 'PHID-USER-admin'); \
             INSERT INTO `{ns}_harbormaster`.harbormaster_buildtarget (id, phid, buildPHID, \
               buildStepPHID, className, details, variables, dateCreated, dateModified, \
               targetStatus, name, dateStarted, dateCompleted, buildGeneration) VALUES \
             (1, 'PHID-HMBT-run', 'PHID-HMBD-1', 'PHID-HMCS-run', \
               'HarbormasterHTTPRequestBuildStepImplementation', \
               '{\"uri\":\"https:\\\\/\\\\/ci.example.test\\\\/run\"}', '{\"build.id\":\"1\"}', \
               2000, 2001, 'target/waiting', 'Run', 2002, NULL, 0); \
             INSERT INTO `{ns}_harbormaster`.harbormaster_buildartifact (id, phid, artifactType, \
               artifactIndex, artifactKey, artifactData, dateCreated, dateModified, \
               buildTargetPHID, isReleased) VALUES \
             (1, 'PHID-HMBA-1', 'uri', 'aaaaaaaaaaaa', 'jobs', \
               '{\"uri\":\"https:\\\\/\\\\/jobs.test\\\\/\"}', 7, 8, 'PHID-HMBT-run', 0)",
        )
        .await;
    Some(Fixture { test_db })
}

impl Fixture {
    fn database(&self) -> &Database {
        self.test_db.database()
    }

    async fn execute(&self, sql: &str) {
        self.test_db.execute(sql).await;
    }

    /// A message from the admin to `receiver`.
    async fn message(&self, receiver: &str, kind: &str) {
        self.execute(&format!(
            "INSERT INTO `{{ns}}_harbormaster`.harbormaster_buildmessage (authorPHID, \
               receiverPHID, type, isConsumed, dateCreated, dateModified) VALUES \
             ('PHID-USER-admin', '{receiver}', '{kind}', 0, 5, 5)"
        ))
        .await;
    }

    async fn schedule(&self, class: &str, data: &Value) -> i64 {
        let data = phab_core::php::json_encode(data);
        let mut connection = self.database().pool().acquire().await.unwrap();
        let id = schedule_task(
            &mut connection,
            self.database(),
            Task {
                class,
                data: &data,
                priority: 2000,
                object_phid: None,
            },
            1_600_000_000,
        )
        .await
        .unwrap();
        i64::try_from(id).unwrap()
    }

    /// Every row of a query, each column as text.
    async fn rows(&self, sql: &str) -> Vec<Row> {
        let sql = sql.replace("{ns}", self.database().namespace());
        let found = sqlx::query(&sql)
            .fetch_all(self.database().pool())
            .await
            .unwrap();
        found
            .iter()
            .map(|row| {
                use sqlx::{Column, Row as _};
                row.columns()
                    .iter()
                    .map(|column| {
                        let value = rows::opt_text(row, column.name())
                            .unwrap()
                            .map_or(Value::Null, Value::String);
                        (column.name().to_owned(), value)
                    })
                    .collect()
            })
            .collect()
    }

    async fn value(&self, sql: &str) -> Value {
        let rows = self.rows(sql).await;
        rows.first()
            .and_then(|row| row.values().next().cloned())
            .unwrap_or(Value::Null)
    }

    fn worker(&self, target: TargetWorker) -> Worker {
        let mut registry = Registry::default();
        registry.register(BuildWorker);
        registry.register(target);
        Worker::new(
            self.database().clone(),
            Config::default(),
            registry,
            LeaseOwner::new("rust-test"),
        )
    }

    /// Runs task `id`; `None` when the handler left it to PHP.
    async fn run_with(&self, id: i64, target: TargetWorker) -> Option<Executed> {
        let worker = self.worker(target);
        let ran = worker.work_on(Some(&[id])).await.unwrap()?;
        assert_eq!(ran.id, id);
        Some(ran.outcome.unwrap())
    }

    async fn run(&self, id: i64) -> Option<Executed> {
        self.run_with(id, TargetWorker::default()).await
    }

    /// The tasks queued, other than `except`: class, object and data.
    async fn queued(&self, except: i64) -> Vec<(String, String, String)> {
        self.rows(&format!(
            "SELECT CAST(task.taskClass AS CHAR) class, CAST(task.objectPHID AS CHAR) object, \
               CAST(task.priority AS CHAR) priority, CAST(data.data AS CHAR) data \
             FROM `{{ns}}_worker`.worker_activetask task \
             JOIN `{{ns}}_worker`.worker_taskdata data ON data.id = task.dataID \
             WHERE task.id != {except} ORDER BY task.id"
        ))
        .await
        .into_iter()
        .map(|row| {
            let text = |key: &str| row[key].as_str().unwrap_or_default().to_owned();
            (
                text("class"),
                format!("{} {}", text("object"), text("priority")),
                text("data"),
            )
        })
        .collect()
    }

    async fn finish(self) {
        self.test_db.drop_databases().await;
    }
}

fn succeeded(outcome: Option<Executed>) {
    match outcome {
        Some(Executed::Succeeded(_)) => {}
        other => panic!("{other:?}"),
    }
}

/// A restart (`HarbormasterBuildMessageRestartTransaction`): the build's
/// artifacts are released, its generation goes up, the transaction is
/// written with its publish task, the message is consumed, then the steps
/// without dependencies get a target of the new generation and its
/// `HarbormasterTargetWorker` task. The build was and stays building, so
/// the buildable isn't updated.
#[tokio::test]
async fn restarts_a_build() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture.message("PHID-HMBD-1", "restart").await;
    let id = fixture
        .schedule("HarbormasterBuildWorker", &json!({"buildID": "1"}))
        .await;
    succeeded(fixture.run(id).await);

    let build = fixture
        .rows(
            "SELECT buildStatus, CAST(buildGeneration AS CHAR) generation FROM \
             `{ns}_harbormaster`.harbormaster_build",
        )
        .await;
    assert_eq!(build[0]["buildStatus"], json!("building"));
    assert_eq!(build[0]["generation"], json!("1"));
    assert_eq!(
        fixture
            .value("SELECT CAST(isReleased AS CHAR) FROM `{ns}_harbormaster`.harbormaster_buildartifact")
            .await,
        json!("1")
    );
    assert_eq!(
        fixture
            .value(
                "SELECT CAST(isConsumed AS CHAR) FROM `{ns}_harbormaster`.harbormaster_buildmessage"
            )
            .await,
        json!("1")
    );
    let xactions = fixture
        .rows(
            "SELECT phid, authorPHID, objectPHID, viewPolicy, editPolicy, transactionType, \
             oldValue, newValue, contentSource, metadata FROM \
             `{ns}_harbormaster`.harbormaster_buildtransaction",
        )
        .await;
    assert_eq!(xactions.len(), 1);
    let xaction = &xactions[0];
    let xaction_phid = xaction["phid"].as_str().unwrap().to_owned();
    assert!(xaction_phid.starts_with("PHID-XACT-HMBD-"));
    assert_eq!(xaction["authorPHID"], json!("PHID-USER-admin"));
    assert_eq!(xaction["objectPHID"], json!("PHID-HMBD-1"));
    assert_eq!(xaction["viewPolicy"], json!("public"));
    assert_eq!(xaction["editPolicy"], json!("no-one"));
    assert_eq!(xaction["transactionType"], json!("message/restart"));
    assert_eq!(xaction["oldValue"], json!("null"));
    assert_eq!(xaction["newValue"], json!("\"restart\""));
    assert_eq!(
        xaction["contentSource"],
        json!(r#"{"source":"daemon","params":[]}"#)
    );
    assert!(
        xaction["metadata"]
            .as_str()
            .unwrap()
            .starts_with(r#"{"core.groupID":""#)
    );

    let targets = fixture
        .rows(
            "SELECT CAST(id AS CHAR) id, phid, name, buildStepPHID, targetStatus, details, \
             variables, CAST(buildGeneration AS CHAR) generation, dateStarted FROM \
             `{ns}_harbormaster`.harbormaster_buildtarget WHERE buildGeneration = 1",
        )
        .await;
    assert_eq!(targets.len(), 1);
    let target = &targets[0];
    assert_eq!(target["name"], json!("Run"));
    assert_eq!(target["buildStepPHID"], json!("PHID-HMCS-run"));
    assert_eq!(target["targetStatus"], json!("target/pending"));
    assert_eq!(target["dateStarted"], Value::Null);
    assert_eq!(
        target["details"],
        json!(r#"{"uri":"https:\/\/ci.example.test\/run?b=${build.id}","method":"POST"}"#)
    );
    let variables: Value = serde_json::from_str(target["variables"].as_str().unwrap()).unwrap();
    let keys: Vec<&str> = variables
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "buildable.diff",
            "buildable.revision",
            "buildable.commit",
            "repository.callsign",
            "repository.phid",
            "repository.vcs",
            "repository.uri",
            "step.timestamp",
            "build.id",
            "initiator.phid",
            "buildable.phid",
            "buildable.object.phid",
            "buildable.container.phid",
            "build.phid",
        ]
    );
    assert_eq!(variables["buildable.diff"], json!("1"));
    assert_eq!(variables["buildable.revision"], json!("1"));
    assert_eq!(variables["build.id"], json!("1"));
    assert!(variables["step.timestamp"].is_i64());
    assert_eq!(variables["initiator.phid"], json!("PHID-USER-admin"));
    assert_eq!(variables["buildable.container.phid"], json!("PHID-DREV-1"));

    let target_id = target["id"].as_str().unwrap();
    let target_phid = target["phid"].as_str().unwrap();
    let queued = fixture.queued(id).await;
    assert_eq!(queued.len(), 2);
    assert_eq!(
        queued[0].0,
        "PhabricatorApplicationTransactionPublishWorker"
    );
    assert_eq!(queued[0].1, "PHID-HMBD-1 1000");
    assert_eq!(
        queued[0].2,
        format!(
            "{{\"objectPHID\":\"PHID-HMBD-1\",\"actorPHID\":\
             \"PHID-APPS-PhabricatorHarbormasterApplication\",\"xactionPHIDs\":[\"{xaction_phid}\"],\
             \"state\":{{\"parentMessageID\":null,\"isNewObject\":false,\"heraldEmailPHIDs\":[],\
             \"heraldForcedEmailPHIDs\":[],\"heraldHeader\":null,\"mailToPHIDs\":[],\
             \"mailCCPHIDs\":[],\"feedNotifyPHIDs\":[],\"feedRelatedPHIDs\":[],\
             \"feedShouldPublish\":false,\"mailShouldSend\":false,\"mustEncrypt\":[],\
             \"mailStamps\":[],\"mailUnexpandablePHIDs\":[],\"mailMutedPHIDs\":[],\
             \"webhookMap\":[],\"silent\":null,\"sendHistory\":false,\
             \"excludeMailRecipientPHIDs\":[],\"custom\":[],\"custom.encoding\":[]}}}}"
        )
    );
    assert_eq!(queued[1].0, "HarbormasterTargetWorker");
    assert_eq!(queued[1].1, format!("{target_phid} 2000"));
    assert_eq!(queued[1].2, format!("{{\"targetID\":{target_id}}}"));
    assert_eq!(
        fixture
            .value("SELECT buildableStatus FROM `{ns}_harbormaster`.harbormaster_buildable")
            .await,
        json!("building")
    );
    fixture.finish().await;
}

/// An abort of a complete build has no effect: the build is saved anyway,
/// the publish task lists no transaction, and the message is consumed.
#[tokio::test]
async fn ignores_commands_without_effect() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .execute(
            "UPDATE `{ns}_harbormaster`.harbormaster_build SET buildStatus = 'failed'; \
             UPDATE `{ns}_harbormaster`.harbormaster_buildartifact SET isReleased = 1",
        )
        .await;
    fixture.message("PHID-HMBD-1", "abort").await;
    let id = fixture
        .schedule("HarbormasterBuildWorker", &json!({"buildID": 1}))
        .await;
    succeeded(fixture.run(id).await);
    assert_eq!(
        fixture
            .value("SELECT CAST(COUNT(*) AS CHAR) FROM `{ns}_harbormaster`.harbormaster_buildtransaction")
            .await,
        json!("0")
    );
    assert_ne!(
        fixture
            .value("SELECT CAST(dateModified AS CHAR) FROM `{ns}_harbormaster`.harbormaster_build")
            .await,
        json!("1001")
    );
    let queued = fixture.queued(id).await;
    assert_eq!(queued.len(), 1);
    assert!(queued[0].2.contains("\"xactionPHIDs\":[]"));
    assert_eq!(
        fixture
            .value(
                "SELECT CAST(isConsumed AS CHAR) FROM `{ns}_harbormaster`.harbormaster_buildmessage"
            )
            .await,
        json!("1")
    );
    fixture.finish().await;
}

/// A `fail` message to the waiting target fails it, which fails the build
/// (a failed step fails it whatever the others do), releases its artifacts
/// and fails the buildable. The revision has no status for the buildable
/// yet, so Differential would publish it: left to PHP.
#[tokio::test]
async fn leaves_revision_publishing_to_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture.message("PHID-HMBT-run", "fail").await;
    let id = fixture
        .schedule("HarbormasterBuildWorker", &json!({"buildID": "1"}))
        .await;
    assert!(fixture.run(id).await.is_none());
    assert_eq!(
        fixture
            .value("SELECT targetStatus FROM `{ns}_harbormaster`.harbormaster_buildtarget")
            .await,
        json!("target/waiting")
    );

    // Once the revision knows the buildable failed, publishing changes
    // nothing.
    fixture
        .execute(
            "UPDATE `{ns}_differential`.differential_revision SET properties = \
             '{\"buildables\":{\"PHID-HMBB-1\":{\"status\":\"failed\"}}}'",
        )
        .await;
    succeeded(fixture.run(id).await);
    let target = fixture
        .rows(
            "SELECT targetStatus, CAST(dateCompleted AS CHAR) completed FROM \
             `{ns}_harbormaster`.harbormaster_buildtarget",
        )
        .await;
    assert_eq!(target[0]["targetStatus"], json!("target/failed"));
    assert!(target[0]["completed"].is_string());
    assert_eq!(
        fixture
            .value("SELECT buildStatus FROM `{ns}_harbormaster`.harbormaster_build")
            .await,
        json!("failed")
    );
    assert_eq!(
        fixture
            .value("SELECT buildableStatus FROM `{ns}_harbormaster`.harbormaster_buildable")
            .await,
        json!("failed")
    );
    assert_eq!(
        fixture
            .value("SELECT CAST(isReleased AS CHAR) FROM `{ns}_harbormaster`.harbormaster_buildartifact")
            .await,
        json!("1")
    );
    assert_eq!(fixture.queued(id).await.len(), 0);
    fixture.finish().await;
}

/// A build which doesn't exist fails permanently, as `loadBuild()` does.
#[tokio::test]
async fn fails_on_missing_builds() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let id = fixture
        .schedule("HarbormasterBuildWorker", &json!({"buildID": "99"}))
        .await;
    match fixture.run(id).await {
        Some(Executed::FailedPermanently { message, .. }) => {
            assert_eq!(message, "Invalid build ID \"99\".");
        }
        other => panic!("{other:?}"),
    }
    fixture.finish().await;
}

/// The HTTP step's URI doesn't resolve: `requireSafeRemoteURIForFetch()`
/// throws, the step logs the silenced call and fails, a log task is
/// queued, and the build fails with it.
#[tokio::test]
async fn fails_http_steps_whose_uri_does_not_resolve() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .execute(
            "UPDATE `{ns}_harbormaster`.harbormaster_buildtarget SET targetStatus = \
               'target/pending', dateStarted = NULL; \
             UPDATE `{ns}_differential`.differential_revision SET properties = \
             '{\"buildables\":{\"PHID-HMBB-1\":{\"status\":\"failed\"}}}'",
        )
        .await;
    let id = fixture
        .schedule("HarbormasterTargetWorker", &json!({"targetID": 1}))
        .await;
    succeeded(fixture.run(id).await);
    let logs = fixture
        .rows(
            "SELECT phid, logSource, logType, CAST(duration AS CHAR) duration, \
             CAST(live AS CHAR) live, CAST(byteLength AS CHAR) byteLength, chunkFormat, lineMap, \
             CAST(id AS CHAR) id FROM `{ns}_harbormaster`.harbormaster_buildlog",
        )
        .await;
    assert_eq!(logs.len(), 1);
    let log = &logs[0];
    let text = "Declining to make service call because `phabricator.silent` is enabled in \
                configuration.";
    assert_eq!(log["logSource"], json!("HTTP Request: Invalid URI"));
    assert_eq!(log["logType"], json!("silenced"));
    assert_eq!(log["duration"], json!("0"));
    assert_eq!(log["live"], json!("0"));
    assert_eq!(log["byteLength"], json!(text.len().to_string()));
    assert_eq!(log["chunkFormat"], json!("text"));
    assert_eq!(
        log["lineMap"],
        json!(format!("[[],{},0,\"Lg==\"]", text.len() - 1))
    );
    let chunk = fixture
        .rows(&format!(
            "SELECT encoding, CAST(headOffset AS CHAR) head, CAST(tailOffset AS CHAR) tail, \
             CAST(size AS CHAR) size, CAST(chunk AS CHAR) chunk FROM \
             `{{ns}}_harbormaster`.harbormaster_buildlogchunk WHERE logID = {}",
            log["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(chunk.len(), 1);
    assert_eq!(chunk[0]["head"], json!("0"));
    assert_eq!(chunk[0]["tail"], json!(text.len().to_string()));
    assert_eq!(chunk[0]["chunk"], json!(text));
    let target = fixture
        .rows(
            "SELECT targetStatus, CAST(dateStarted AS CHAR) started, \
             CAST(dateCompleted AS CHAR) completed FROM \
             `{ns}_harbormaster`.harbormaster_buildtarget",
        )
        .await;
    assert_eq!(target[0]["targetStatus"], json!("target/failed"));
    assert!(target[0]["started"].is_string());
    assert!(target[0]["completed"].is_string());
    assert_eq!(
        fixture
            .value("SELECT buildStatus FROM `{ns}_harbormaster`.harbormaster_build")
            .await,
        json!("failed")
    );
    let queued = fixture.queued(id).await;
    let log_phid = log["phid"].as_str().unwrap();
    assert_eq!(
        queued,
        [(
            "HarbormasterLogWorker".to_owned(),
            format!("{log_phid} 2000"),
            format!("{{\"logPHID\":\"{log_phid}\",\"force\":false}}"),
        )]
    );
    fixture.finish().await;
}

/// Answers every request with a fixed response.
struct Answer;

impl HttpClient for Answer {
    fn send<'a>(&'a self, request: &'a HttpRequest) -> HttpFuture<'a> {
        assert_eq!(request.method, "POST");
        assert_eq!(request.uri, "http://127.0.0.1:9/run");
        Box::pin(async {
            HttpResponse {
                status: 200,
                transport_error: false,
                headers: vec![("Server".to_owned(), "test".to_owned())],
                body: b"ok".to_vec(),
            }
        })
    }
}

/// Requests go through the worker's client, and are left to PHP without
/// one: here the answer is a success, logged as a head and a body log, and
/// the step waits for its message.
#[tokio::test]
async fn sends_requests_through_the_client() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .execute(
            "UPDATE `{ns}_harbormaster`.harbormaster_buildtarget SET targetStatus = \
               'target/pending', details = \
               '{\"uri\":\"http:\\\\/\\\\/127.0.0.1:9\\\\/run\",\"content_type\":\"\",\
               \"http_body\":\"\",\"builtin.wait-for-message\":\"wait\"}'; \
             UPDATE `{ns}_differential`.differential_revision SET properties = \
             '{\"buildables\":{\"PHID-HMBB-1\":{\"status\":\"failed\"}}}'",
        )
        .await;
    let id = fixture
        .schedule("HarbormasterTargetWorker", &json!({"targetID": 1}))
        .await;
    // 127.0.0.1 is blacklisted by default: the call is silenced.
    let mut config = Config::default();
    let mut layer = Map::new();
    layer.insert("security.outbound-blacklist".to_owned(), json!([]));
    config.push_layer("test", layer);
    let worker = |target: TargetWorker| {
        let mut registry = Registry::default();
        registry.register(target);
        Worker::new(
            fixture.database().clone(),
            config.clone(),
            registry,
            LeaseOwner::new("rust-test"),
        )
    };
    assert!(
        worker(TargetWorker::default())
            .work_on(Some(&[id]))
            .await
            .unwrap()
            .is_none()
    );
    let ran = worker(TargetWorker::with_client(Arc::new(Answer)))
        .work_on(Some(&[id]))
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(ran.outcome, Ok(Executed::Succeeded(_))),
        "{:?}",
        ran.outcome
    );
    let logs = fixture
        .rows(
            "SELECT logSource, logType, CAST(byteLength AS CHAR) length FROM \
             `{ns}_harbormaster`.harbormaster_buildlog ORDER BY id",
        )
        .await;
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[0]["logSource"], json!("http://127.0.0.1:9/run"));
    assert_eq!(logs[0]["logType"], json!("http.head"));
    assert_eq!(logs[0]["length"], json!("21"));
    assert_eq!(logs[1]["logType"], json!("http.body"));
    assert_eq!(logs[1]["length"], json!("2"));
    assert_eq!(
        fixture
            .value("SELECT targetStatus FROM `{ns}_harbormaster`.harbormaster_buildtarget")
            .await,
        json!("target/waiting")
    );
    fixture.finish().await;
}
