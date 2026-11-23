//! The rows the worker writes, against a throwaway `MySQL` database: the
//! cases of PHP's `PhabricatorWorkerTestCase`, plus those PHP leaves
//! untested (class filtering, expired leases, yields, follow-ups).

use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_db::worker::{Task, schedule_task};
use phab_db::{Database, rows};
use phab_worker::execute::ExecuteError;
use phab_worker::handlers::test_worker::TestWorker;
use phab_worker::lease::{DEFAULT_LEASE_DURATION, DEFAULT_WAIT_BEFORE_RETRY};
use phab_worker::{
    ActiveTask, Executed, Failure, Handler, LeaseOwner, LeaseQuery, QueuedTask, Registry,
    TaskResult, Work, WorkFuture, Worker, execute,
};
use serde_json::{Value, json};

const TEST_CLASS: &str = "PhabricatorTestWorker";

struct Fixture {
    test_db: TestDatabase,
    owner: LeaseOwner,
    config: Config,
}

async fn fixture() -> Option<Fixture> {
    let test_db = TestDatabase::create(&["worker"]).await?;
    Some(Fixture {
        test_db,
        owner: LeaseOwner::new("rust-test"),
        config: Config::default(),
    })
}

impl Fixture {
    fn database(&self) -> &Database {
        self.test_db.database()
    }

    /// `PhabricatorWorker::scheduleTask()`.
    async fn schedule(&self, class: &str, data: &Value, priority: Option<i64>) -> i64 {
        let data = phab_core::php::json_encode(data);
        let mut connection = self.database().pool().acquire().await.unwrap();
        let id = schedule_task(
            &mut connection,
            self.database(),
            Task {
                class,
                data: &data,
                priority: priority.unwrap_or(phab_worker::PRIORITY_DEFAULT),
                object_phid: None,
            },
            phab_worker_time(),
        )
        .await
        .unwrap();
        i64::try_from(id).unwrap()
    }

    async fn schedule_test(&self, data: Value) -> i64 {
        self.schedule(TEST_CLASS, &data, None).await
    }

    async fn lease_classes(&self, classes: &[&str]) -> Vec<ActiveTask> {
        LeaseQuery {
            classes,
            ids: None,
            object_phids: None,
            limit: 1,
            inspection: None,
        }
        .execute(self.database(), &self.owner)
        .await
        .unwrap()
    }

    /// `expectNextLease()`.
    async fn expect_next_lease(&self, id: Option<i64>) -> Option<ActiveTask> {
        let mut leased = self.lease_classes(&[TEST_CLASS]).await;
        assert_eq!(
            leased.iter().map(|task| task.id).collect::<Vec<_>>(),
            id.into_iter().collect::<Vec<_>>()
        );
        leased.pop()
    }

    async fn run(&self, task: ActiveTask, handler: &dyn Handler) -> Result<Executed, ExecuteError> {
        execute(task, handler, self.database(), &self.config).await
    }

    async fn schedule_and_execute(&self, data: Value) -> Executed {
        let id = self.schedule_test(data).await;
        let task = self.expect_next_lease(Some(id)).await.unwrap();
        self.run(task, &TestWorker).await.unwrap()
    }

    async fn server_time(&self) -> i64 {
        let row = sqlx::query("SELECT UNIX_TIMESTAMP() now")
            .fetch_one(self.database().pool())
            .await
            .unwrap();
        rows::int(&row, "now").unwrap()
    }

    /// Every column of a row, as text, for comparisons.
    async fn row(&self, table: &str, id: i64) -> Option<serde_json::Map<String, Value>> {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT CAST(COLUMN_NAME AS CHAR) FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
        )
        .bind(format!("{}_worker", self.database().namespace()))
        .bind(table)
        .fetch_all(self.database().pool())
        .await
        .unwrap();
        let select = columns
            .iter()
            .map(|column| format!("CAST(`{column}` AS CHAR) `{column}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let row = sqlx::query(&format!(
            "SELECT {select} FROM {} WHERE id = ?",
            self.database().table("worker", table)
        ))
        .bind(id)
        .fetch_optional(self.database().pool())
        .await
        .unwrap()?;
        Some(
            columns
                .iter()
                .map(|column| {
                    let value = rows::opt_text(&row, column)
                        .unwrap()
                        .map_or(Value::Null, Value::String);
                    (column.clone(), value)
                })
                .collect(),
        )
    }

    async fn active(&self, id: i64) -> Option<serde_json::Map<String, Value>> {
        self.row("worker_activetask", id).await
    }

    async fn archived(&self, id: i64) -> Option<serde_json::Map<String, Value>> {
        self.row("worker_archivetask", id).await
    }

    async fn set(&self, id: i64, assignments: &str) {
        self.test_db
            .execute(&format!(
                "UPDATE `{{ns}}_worker`.worker_activetask SET {assignments} WHERE id = {id}"
            ))
            .await;
    }

    async fn finish(self) {
        self.test_db.drop_databases().await;
    }
}

fn phab_worker_time() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn int(row: &serde_json::Map<String, Value>, column: &str) -> Option<i64> {
    row[column].as_str().map(|text| text.parse().unwrap())
}

fn text<'a>(row: &'a serde_json::Map<String, Value>, column: &str) -> Option<&'a str> {
    row[column].as_str()
}

/// Asserts `value` is within a few seconds of `expected` (the clocks move
/// while the test runs).
fn assert_near(value: Option<i64>, expected: i64) {
    let value = value.expect("a value");
    assert!(
        (value - expected).abs() <= 3,
        "{value} is not close to {expected}"
    );
}

#[tokio::test]
async fn lease_task() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let id = fixture.schedule_test(json!([])).await;
    let task = fixture.expect_next_lease(Some(id)).await.unwrap();
    assert_eq!(task.data, json!([]));
    assert_eq!(task.failure_count, 0);
    let row = fixture.active(id).await.unwrap();
    let owner = text(&row, "leaseOwner").unwrap();
    assert_eq!(Some(owner), task.lease_owner.as_deref());
    assert!(owner.ends_with(":rust-test:1"), "{owner}");
    assert_near(
        int(&row, "leaseExpires"),
        fixture.server_time().await + DEFAULT_LEASE_DURATION,
    );
    // testMultipleLease
    fixture.expect_next_lease(None).await;
    fixture.finish().await;
}

#[tokio::test]
async fn leases_oldest_and_lowest_priority_first() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // testOldestFirst
    let first = fixture.schedule_test(json!([])).await;
    let second = fixture.schedule_test(json!([])).await;
    fixture.expect_next_lease(Some(first)).await;
    fixture.expect_next_lease(Some(second)).await;
    // testLeasedIsLowestPriority
    let task1 = fixture.schedule(TEST_CLASS, &json!([]), Some(2)).await;
    let task2 = fixture.schedule(TEST_CLASS, &json!([]), Some(2)).await;
    let task3 = fixture.schedule(TEST_CLASS, &json!([]), Some(1)).await;
    fixture.expect_next_lease(Some(task3)).await;
    fixture.expect_next_lease(Some(task1)).await;
    fixture.expect_next_lease(Some(task2)).await;
    fixture.finish().await;
}

#[tokio::test]
async fn leases_new_tasks_before_expired_ones() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // testNewBeforeLeased
    let task1 = fixture.schedule_test(json!([])).await;
    let task2 = fixture.schedule_test(json!([])).await;
    let now = phab_worker_time();
    fixture
        .set(
            task1,
            &format!("leaseOwner = 'test', leaseExpires = {}", now - 100_000),
        )
        .await;
    fixture.expect_next_lease(Some(task2)).await;
    fixture.expect_next_lease(Some(task1)).await;
    // testLeasedIsOldestFirst
    let task3 = fixture.schedule_test(json!([])).await;
    let task4 = fixture.schedule_test(json!([])).await;
    fixture
        .set(
            task3,
            &format!("leaseOwner = 'test', leaseExpires = {}", now - 100_000),
        )
        .await;
    fixture
        .set(
            task4,
            &format!("leaseOwner = 'test', leaseExpires = {}", now - 200_000),
        )
        .await;
    fixture.expect_next_lease(Some(task4)).await;
    fixture.expect_next_lease(Some(task3)).await;
    fixture.finish().await;
}

#[tokio::test]
async fn never_leases_classes_without_a_handler() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // A PHP task ahead of ours in priority, and one with an expired lease.
    let php_new = fixture
        .schedule(
            "PhabricatorSearchWorker",
            &json!({"documentPHID": "PHID-X"}),
            Some(1),
        )
        .await;
    let php_expired = fixture
        .schedule("PhabricatorMetaMTAWorker", &json!([]), Some(1))
        .await;
    fixture
        .set(
            php_expired,
            &format!(
                "leaseOwner = '1:2:php:1', leaseExpires = {}",
                phab_worker_time() - 1000
            ),
        )
        .await;
    let php_rebuild = fixture
        .schedule(
            "PhabricatorRebuildIndexesWorker",
            &json!({"queryClass": "HeraldRuleQuery"}),
            Some(3500),
        )
        .await;
    let ids = [php_new, php_expired, php_rebuild];
    let mut before = Vec::new();
    for id in ids {
        before.push(fixture.active(id).await.unwrap());
    }

    let ours = fixture.schedule_test(json!([])).await;
    let worker = Worker::new(
        fixture.database().clone(),
        Config::default(),
        phab_worker::handlers::registry(),
        LeaseOwner::new("rust-test"),
    );
    assert!(worker.work_once().await.unwrap());
    assert!(fixture.archived(ours).await.is_some());
    assert!(!worker.work_once().await.unwrap());

    for (id, row) in ids.iter().zip(before) {
        assert_eq!(fixture.active(*id).await.unwrap(), row, "task {id}");
        assert!(fixture.archived(*id).await.is_none());
    }
    // An empty registry leases nothing at all.
    assert!(fixture.lease_classes(&[]).await.is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn success_archives_the_task() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // testExecuteTask
    let id = fixture.schedule_test(json!({"x": 1})).await;
    fixture
        .set(
            id,
            "objectPHID = 'PHID-DREV-1', containerPHID = 'PHID-REPO-1', dateCreated = 1000",
        )
        .await;
    let before = fixture.active(id).await.unwrap();
    let task = fixture.expect_next_lease(Some(id)).await.unwrap();
    let leased = fixture.active(id).await.unwrap();
    let Executed::Succeeded(archive) = fixture.run(task, &TestWorker).await.unwrap() else {
        panic!("expected a success");
    };
    assert_eq!(archive.result, TaskResult::Success);

    assert!(fixture.active(id).await.is_none());
    let row = fixture.archived(id).await.unwrap();
    let now = phab_worker_time();
    for column in [
        "taskClass",
        "failureCount",
        "dataID",
        "priority",
        "objectPHID",
    ] {
        assert_eq!(row[column], before[column], "{column}");
    }
    assert_eq!(text(&row, "containerPHID"), Some("PHID-REPO-1"));
    assert_eq!(row["leaseOwner"], leased["leaseOwner"]);
    assert_eq!(row["leaseExpires"], leased["leaseExpires"]);
    assert_eq!(int(&row, "result"), Some(0));
    assert!(int(&row, "duration").unwrap() >= 0);
    assert_eq!(int(&row, "dateCreated"), Some(1000));
    assert_near(int(&row, "dateModified"), now);
    assert_near(int(&row, "archivedEpoch"), now);
    // The data row stays; `bin/garbage` collects it with the archive.
    let data_id = int(&row, "dataID").unwrap();
    let data = sqlx::query(&format!(
        "SELECT data FROM {} WHERE id = ?",
        fixture.database().table("worker", "worker_taskdata")
    ))
    .bind(data_id)
    .fetch_one(fixture.database().pool())
    .await
    .unwrap();
    assert_eq!(rows::text(&data, "data").unwrap(), r#"{"x":1}"#);
    fixture.finish().await;
}

#[tokio::test]
async fn permanent_failure_archives_with_failure() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // testPermanentTaskFailure
    let executed = fixture
        .schedule_and_execute(json!({"doWork": "fail-permanent"}))
        .await;
    let Executed::FailedPermanently { archive, message } = executed else {
        panic!("expected a permanent failure");
    };
    assert_eq!(message, "Permanent failure!");
    let row = fixture.archived(archive.task.id).await.unwrap();
    assert_eq!(int(&row, "result"), Some(1));
    assert_eq!(int(&row, "duration"), Some(0));
    assert_eq!(int(&row, "failureCount"), Some(0));
    assert!(fixture.active(archive.task.id).await.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn temporary_failure_retries_later() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // testTemporaryTaskFailure
    let id = fixture
        .schedule_test(json!({"doWork": "fail-temporary"}))
        .await;
    let task = fixture.expect_next_lease(Some(id)).await.unwrap();
    let server_time = task.server_time;
    let owner = task.lease_owner.clone();
    let Executed::Failed { task, message } = fixture.run(task, &TestWorker).await.unwrap() else {
        panic!("expected a failure");
    };
    assert_eq!(message, "Temporary failure!");
    assert_eq!(task.failure_count, 1);
    let row = fixture.active(id).await.unwrap();
    assert_eq!(int(&row, "failureCount"), Some(1));
    assert_near(int(&row, "failureTime"), phab_worker_time());
    assert_near(int(&row, "dateModified"), phab_worker_time());
    // The wait counts from the lease, not from the failure.
    assert_eq!(
        int(&row, "leaseExpires"),
        Some(server_time + DEFAULT_WAIT_BEFORE_RETRY)
    );
    assert_eq!(text(&row, "leaseOwner"), owner.as_deref());
    assert!(fixture.archived(id).await.is_none());
    // testWaitBeforeRetry: not leased again until the wait is over.
    fixture.expect_next_lease(None).await;
    fixture.finish().await;
}

#[tokio::test]
async fn too_many_failures_fail_permanently() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // testTooManyTaskFailures
    let id = fixture
        .schedule_test(json!({
            "doWork": "fail-temporary",
            "getMaximumRetryCount": 3,
            "getWaitBeforeRetry": -60,
        }))
        .await;
    for failures in 1..=4 {
        let task = fixture.expect_next_lease(Some(id)).await.unwrap();
        let server_time = task.server_time;
        let Executed::Failed { task, .. } = fixture.run(task, &TestWorker).await.unwrap() else {
            panic!("expected failure {failures}");
        };
        assert_eq!(task.failure_count, failures);
        let row = fixture.active(id).await.unwrap();
        assert_eq!(int(&row, "failureCount"), Some(failures));
        assert_eq!(int(&row, "leaseExpires"), Some(server_time - 60));
    }
    let task = fixture.expect_next_lease(Some(id)).await.unwrap();
    let Executed::FailedPermanently { message, .. } = fixture.run(task, &TestWorker).await.unwrap()
    else {
        panic!("expected a permanent failure");
    };
    assert_eq!(
        message,
        format!("Task {id} has exceeded the maximum number of failures (3).")
    );
    let row = fixture.archived(id).await.unwrap();
    assert_eq!(int(&row, "result"), Some(1));
    assert_eq!(int(&row, "failureCount"), Some(4));
    assert_eq!(int(&row, "duration"), Some(0));
    fixture.finish().await;
}

#[tokio::test]
async fn required_lease_time_extends_the_lease() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // testRequiredLeaseTime
    let Executed::Succeeded(archive) = fixture
        .schedule_and_execute(json!({"getRequiredLeaseTime": 1_000_000}))
        .await
    else {
        panic!("expected a success");
    };
    let row = fixture.archived(archive.task.id).await.unwrap();
    assert!(int(&row, "leaseExpires").unwrap() - phab_worker_time() > 1000);
    assert_eq!(
        int(&row, "leaseExpires"),
        Some(archive.task.server_time + 1_000_000)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn takes_over_tasks_whose_lease_expired() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // A worker died holding the task: it is ours once its lease is over.
    let id = fixture.schedule_test(json!([])).await;
    fixture
        .set(
            id,
            &format!(
                "leaseOwner = '99:1:dead-host:1', leaseExpires = {}",
                phab_worker_time() - 10
            ),
        )
        .await;
    let task = fixture.expect_next_lease(Some(id)).await.unwrap();
    let owner = task.lease_owner.clone().unwrap();
    assert_ne!(owner, "99:1:dead-host:1");
    fixture.run(task, &TestWorker).await.unwrap();
    let row = fixture.archived(id).await.unwrap();
    assert_eq!(text(&row, "leaseOwner"), Some(owner.as_str()));
    assert_eq!(int(&row, "result"), Some(0));

    // A lease still running is left alone.
    let held = fixture.schedule_test(json!([])).await;
    fixture
        .set(
            held,
            &format!(
                "leaseOwner = '99:1:live-host:1', leaseExpires = {}",
                phab_worker_time() + 600
            ),
        )
        .await;
    fixture.expect_next_lease(None).await;
    fixture.finish().await;
}

#[tokio::test]
async fn expired_lease_writes_nothing() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // The lease runs out while the task runs: `archiveTask()` and the
    // failure path both refuse, so the row keeps what was last saved and
    // the task is retried once the lease is over, with no failure counted.
    let id = fixture
        .schedule_test(json!({"getRequiredLeaseTime": -10}))
        .await;
    let task = fixture.expect_next_lease(Some(id)).await.unwrap();
    let server_time = task.server_time;
    let owner = task.lease_owner.clone();
    let error = fixture.run(task, &TestWorker).await.unwrap_err();
    assert!(matches!(error, ExecuteError::LeaseExpired(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        format!("Trying to update Task {id} ({TEST_CLASS}) after lease expiration!")
    );
    let row = fixture.active(id).await.unwrap();
    assert_eq!(int(&row, "leaseExpires"), Some(server_time - 10));
    assert_eq!(text(&row, "leaseOwner"), owner.as_deref());
    assert_eq!(int(&row, "failureCount"), Some(0));
    assert_eq!(row["failureTime"], Value::Null);
    assert!(fixture.archived(id).await.is_none());
    fixture.expect_next_lease(Some(id)).await;
    fixture.finish().await;
}

/// Yields, or queues a follow-up, as its data says.
struct ScriptedWorker;

impl Handler for ScriptedWorker {
    fn class(&self) -> &'static str {
        TEST_CLASS
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            if let Some(duration) = work.data().get("yield").and_then(Value::as_i64) {
                return Err(Failure::Yield(duration));
            }
            work.queue_task(QueuedTask {
                class: "PhabricatorSearchWorker".to_owned(),
                data: json!({"documentPHID": "PHID-DREV-1"}),
                priority: None,
                object_phid: Some("PHID-DREV-1".to_owned()),
                container_phid: None,
            });
            if work.data().get("fail").is_some() {
                return Err(Failure::Temporary("no".to_owned()));
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn yield_requeues_without_a_failure() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let id = fixture.schedule_test(json!({"yield": 1})).await;
    let task = fixture.expect_next_lease(Some(id)).await.unwrap();
    let server_time = task.server_time;
    let Executed::Yielded(_) = fixture.run(task, &ScriptedWorker).await.unwrap() else {
        panic!("expected a yield");
    };
    let row = fixture.active(id).await.unwrap();
    assert_eq!(text(&row, "leaseOwner"), Some("(yield)"));
    // At least 5 seconds.
    assert_eq!(int(&row, "leaseExpires"), Some(server_time + 5));
    assert_eq!(int(&row, "failureCount"), Some(0));
    assert_eq!(row["failureTime"], Value::Null);
    fixture.expect_next_lease(None).await;
    fixture.finish().await;
}

#[tokio::test]
async fn follow_ups_are_queued_only_on_success() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let count = || async {
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {} WHERE taskClass = 'PhabricatorSearchWorker'",
            fixture.database().table("worker", "worker_activetask")
        ))
        .fetch_one(fixture.database().pool())
        .await
        .unwrap();
        total
    };
    let failing = fixture
        .schedule(TEST_CLASS, &json!({"fail": 1}), Some(1234))
        .await;
    let task = fixture.expect_next_lease(Some(failing)).await.unwrap();
    fixture.run(task, &ScriptedWorker).await.unwrap();
    assert_eq!(count().await, 0);

    let id = fixture.schedule(TEST_CLASS, &json!([]), Some(1234)).await;
    let task = fixture.expect_next_lease(Some(id)).await.unwrap();
    fixture.run(task, &ScriptedWorker).await.unwrap();
    assert_eq!(count().await, 1);
    let row = sqlx::query(&format!(
        "SELECT task.priority, task.objectPHID, data.data FROM {} task JOIN {} data \
         ON data.id = task.dataID WHERE taskClass = 'PhabricatorSearchWorker'",
        fixture.database().table("worker", "worker_activetask"),
        fixture.database().table("worker", "worker_taskdata"),
    ))
    .fetch_one(fixture.database().pool())
    .await
    .unwrap();
    assert_eq!(rows::int(&row, "priority").unwrap(), 1234);
    assert_eq!(
        rows::opt_text(&row, "objectPHID").unwrap().as_deref(),
        Some("PHID-DREV-1")
    );
    assert_eq!(
        rows::text(&row, "data").unwrap(),
        r#"{"documentPHID":"PHID-DREV-1"}"#
    );
    fixture.finish().await;
}

#[tokio::test]
async fn filters_on_ids_and_object_phids() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let first = fixture.schedule_test(json!([])).await;
    let second = fixture.schedule_test(json!([])).await;
    fixture.set(second, "objectPHID = 'PHID-DREV-2'").await;
    let phids = ["PHID-DREV-2".to_owned()];
    let leased = LeaseQuery {
        classes: &[TEST_CLASS],
        ids: None,
        object_phids: Some(&phids),
        limit: 5,
        inspection: None,
    }
    .execute(fixture.database(), &fixture.owner)
    .await
    .unwrap();
    assert_eq!(
        leased.iter().map(|task| task.id).collect::<Vec<_>>(),
        [second]
    );
    let leased = LeaseQuery {
        classes: &[TEST_CLASS],
        ids: Some(&[first, 3]),
        object_phids: None,
        limit: 5,
        inspection: None,
    }
    .execute(fixture.database(), &fixture.owner)
    .await
    .unwrap();
    assert_eq!(
        leased.iter().map(|task| task.id).collect::<Vec<_>>(),
        [first]
    );
    let mut registry = Registry::default();
    registry.register(TestWorker);
    assert_eq!(registry.classes(), [TEST_CLASS]);
    fixture.finish().await;
}
