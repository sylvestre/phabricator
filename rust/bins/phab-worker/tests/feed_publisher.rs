//! `FeedPublisherWorker` against a throwaway `MySQL` database: the rows it
//! leaves, compared to those PHP's `FeedPublisherWorker::doWork()` and
//! `PhabricatorWorkerActiveTask::executeTask()` write (derived from the PHP
//! code, see each test), and the tasks it leaves to PHP untouched.
//!
//! The task data is what `PhabricatorFeedStoryPublisher::publish()` queues:
//! `{"key": <chronological key>}`, an integer.

use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_db::worker::{Task, schedule_task};
use phab_db::{Database, rows};
use phab_worker::handlers::feed_publisher::FeedPublisherWorker;
use phab_worker::{Executed, LeaseOwner, LeaseQuery, Registry, Worker, execute};
use serde_json::{Map, Value, json};

const CLASS: &str = "FeedPublisherWorker";
const STORY: &str = "PhabricatorApplicationTransactionFeedStory";
const DATABASES: &[&str] = &[
    "worker",
    "feed",
    "differential",
    "repository",
    "project",
    "user",
];
const EPOCH: u64 = 1_700_000_000;

fn key(id: u64) -> u64 {
    (EPOCH << 32) + id
}

type Row = Map<String, Value>;

struct Fixture {
    test_db: TestDatabase,
}

async fn fixture() -> Option<Fixture> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{ns}_differential`.differential_revision (title, phid, status, summary, \
               testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, viewPolicy, \
               editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             ('a', 'PHID-DREV-one', 'needs-review', '', '', 'PHID-USER-alice', 1, 1, '{}', 'k', \
               'no-one', 'no-one', 'PHID-REPO-gone', '{}', ''); \
             INSERT INTO `{ns}_project`.project (id, name, phid, authorPHID, dateCreated, \
               dateModified, status, viewPolicy, editPolicy, joinPolicy, icon, color, mailKey, \
               hasWorkboard, hasMilestones, hasSubprojects, projectPath, projectDepth, \
               projectPathKey, properties, subtype, primarySlug, parentProjectPHID, \
               milestoneNumber) VALUES \
             (1, 'Build', 'PHID-PROJ-build', 'x', 1, 1, '0', 'no-one', 'no-one', 'no-one', \
               'fa-cog', 'blue', 'k', 0, 0, 0, 'a', 0, 'k001', '{}', 'default', 'build', NULL, NULL), \
             (2, 'Orphan', 'PHID-PROJ-orphan', 'x', 1, 1, '0', 'users', 'users', 'users', 'fa-x', \
               'red', 'k', 0, 0, 0, 'b', 1, 'k002', '{}', 'default', NULL, 'PHID-PROJ-gone', NULL)",
        )
        .await;
    Some(Fixture { test_db })
}

impl Fixture {
    fn database(&self) -> &Database {
        self.test_db.database()
    }

    /// A story about `object` with these transactions, referenced by it and
    /// its author; `data` replaces the story data if given.
    async fn story(&self, id: u64, object: &str, transactions: &[&str], data: Option<&str>) {
        let phids: Map<String, Value> = transactions
            .iter()
            .map(|phid| ((*phid).to_owned(), Value::from(*phid)))
            .collect();
        let default = json!({
            "objectPHID": object,
            "transactionPHIDs": phids,
        })
        .to_string();
        let data = data.unwrap_or(&default);
        let key = key(id);
        self.test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_feed`.feed_storydata (id, phid, chronologicalKey, storyType, \
                   storyData, authorPHID, dateCreated, dateModified) VALUES \
                 ({id}, 'PHID-STRY-{id}', {key}, '{STORY}', '{data}', 'PHID-USER-alice', 1, 1); \
                 INSERT IGNORE INTO `{{ns}}_feed`.feed_storyreference \
                   (objectPHID, chronologicalKey) \
                 VALUES ('{object}', {key}), ('PHID-USER-alice', {key})"
            ))
            .await;
    }

    /// A transaction of `object` in `table`.
    async fn transaction(
        &self,
        table: &str,
        phid: &str,
        object: &str,
        kind: &str,
        values: (&str, &str),
        metadata: &str,
    ) {
        let (old, new) = values;
        self.test_db
            .execute(&format!(
                "INSERT INTO `{{ns}}_{table}` (phid, authorPHID, objectPHID, viewPolicy, \
                   editPolicy, commentPHID, commentVersion, transactionType, oldValue, newValue, \
                   contentSource, metadata, dateCreated, dateModified) VALUES \
                 ('{phid}', 'PHID-USER-alice', '{object}', 'public', 'PHID-USER-alice', NULL, 0, \
                   '{kind}', '{old}', '{new}', '{{}}', '{metadata}', 1, 1)"
            ))
            .await;
    }

    async fn schedule(&self, data: &Value, priority: i64) -> i64 {
        let data = phab_core::php::json_encode(data);
        let mut connection = self.database().pool().acquire().await.unwrap();
        let id = schedule_task(
            &mut connection,
            self.database(),
            Task {
                class: CLASS,
                data: &data,
                priority,
                object_phid: None,
            },
            1_600_000_000,
        )
        .await
        .unwrap();
        i64::try_from(id).unwrap()
    }

    fn worker(&self, config: Config) -> Worker {
        let mut registry = Registry::default();
        registry.register(FeedPublisherWorker);
        Worker::new(
            self.database().clone(),
            config,
            registry,
            LeaseOwner::new("rust-test"),
        )
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

    /// The columns of `worker_activetask` or `worker_archivetask` as text.
    async fn task_row(&self, table: &str, id: i64) -> Option<Row> {
        self.rows(&format!(
            "SELECT CAST(taskClass AS CHAR) taskClass, CAST(leaseOwner AS CHAR) leaseOwner, \
               CAST(leaseExpires AS CHAR) leaseExpires, CAST(failureCount AS CHAR) failureCount, \
               CAST(dataID AS CHAR) dataID, CAST(priority AS CHAR) priority, \
               CAST(objectPHID AS CHAR) objectPHID, CAST(containerPHID AS CHAR) containerPHID, \
               CAST(dateCreated AS CHAR) dateCreated, CAST(dateModified AS CHAR) dateModified{} \
             FROM `{{ns}}_worker`.{table} WHERE id = {id}",
            if table == "worker_archivetask" {
                ", CAST(result AS CHAR) result, CAST(duration AS CHAR) duration"
            } else {
                ", CAST(failureTime AS CHAR) failureTime"
            }
        ))
        .await
        .pop()
    }

    /// Every task other than `except`, with its data: what was queued.
    async fn other_tasks(&self, except: i64) -> Vec<Row> {
        self.rows(&format!(
            "SELECT CAST(task.id AS CHAR) id, CAST(taskClass AS CHAR) taskClass, \
               CAST(leaseOwner AS CHAR) leaseOwner, CAST(leaseExpires AS CHAR) leaseExpires, \
               CAST(failureCount AS CHAR) failureCount, CAST(failureTime AS CHAR) failureTime, \
               CAST(priority AS CHAR) priority, CAST(objectPHID AS CHAR) objectPHID, \
               CAST(containerPHID AS CHAR) containerPHID, CAST(data.data AS CHAR) data \
             FROM `{{ns}}_worker`.worker_activetask task \
             JOIN `{{ns}}_worker`.worker_taskdata data ON data.id = task.dataID \
             WHERE task.id != {except} ORDER BY task.id"
        ))
        .await
    }

    /// Runs the queue once; returns the archived row of `id`.
    async fn run(&self, config: Config, id: i64) -> Row {
        let worker = self.worker(config);
        let ran = worker.work_on(None).await.unwrap().expect("a task to run");
        assert_eq!(ran.id, id);
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        assert!(self.task_row("worker_activetask", id).await.is_none());
        self.task_row("worker_archivetask", id).await.unwrap()
    }

    async fn finish(self) {
        self.test_db.drop_databases().await;
    }
}

fn text<'a>(row: &'a Row, column: &str) -> Option<&'a str> {
    row[column].as_str()
}

fn hooks(uris: &[&str]) -> Config {
    let mut config = Config::default();
    let mut layer = Map::new();
    layer.insert("feed.http-hooks".to_owned(), json!(uris));
    config.push_layer("test", layer);
    config
}

/// `executeTask()` archives with `RESULT_SUCCESS` (0) after `doWork()`
/// returns, keeping the row's columns; a permanent failure archives with
/// `RESULT_FAILURE` (1) and a duration of 0.
fn assert_archived(row: &Row, result: &str) {
    assert_eq!(text(row, "taskClass"), Some(CLASS));
    assert_eq!(text(row, "failureCount"), Some("0"));
    assert_eq!(text(row, "result"), Some(result));
    assert_eq!(text(row, "objectPHID"), None);
    assert_eq!(text(row, "containerPHID"), None);
    assert_eq!(text(row, "dateCreated"), Some("1600000000"));
    if result == "1" {
        assert_eq!(text(row, "duration"), Some("0"));
    }
}

#[tokio::test]
async fn queues_an_http_task_per_hook() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // The revision is visible to no one and its repository is gone: the
    // omnipotent viewer loads it anyway.
    fixture
        .transaction(
            "differential`.`differential_transaction",
            "PHID-XACT-DREV-1",
            "PHID-DREV-one",
            "differential.revision.accept",
            ("null", "true"),
            "{}",
        )
        .await;
    fixture
        .story(1, "PHID-DREV-one", &["PHID-XACT-DREV-1"], None)
        .await;
    let id = fixture.schedule(&json!({"key": key(1)}), 1000).await;

    let archived = fixture
        .run(hooks(&["http://hooks.example/a", "https://b/"]), id)
        .await;
    assert_archived(&archived, "0");
    assert_eq!(text(&archived, "priority"), Some("1000"));

    // `queueTask('FeedPublisherHTTPWorker', array('key' => ..., 'uri' =>
    // ...))` with the key of the loaded row (a string), scheduled by
    // `flushTaskQueue(array('priority' => <ours>))`: no object, no lease.
    let queued = fixture.other_tasks(id).await;
    let expected: Vec<(String, String)> = ["http:\\/\\/hooks.example\\/a", "https:\\/\\/b\\/"]
        .iter()
        .map(|uri| {
            (
                "FeedPublisherHTTPWorker".to_owned(),
                format!("{{\"key\":\"{}\",\"uri\":\"{uri}\"}}", key(1)),
            )
        })
        .collect();
    assert_eq!(
        queued
            .iter()
            .map(|row| (
                text(row, "taskClass").unwrap().to_owned(),
                text(row, "data").unwrap().to_owned()
            ))
            .collect::<Vec<_>>(),
        expected
    );
    for row in &queued {
        assert_eq!(text(row, "priority"), Some("1000"));
        assert_eq!(text(row, "leaseOwner"), None);
        assert_eq!(text(row, "leaseExpires"), None);
        assert_eq!(text(row, "failureCount"), Some("0"));
        assert_eq!(text(row, "failureTime"), None);
        assert_eq!(text(row, "objectPHID"), None);
        assert_eq!(text(row, "containerPHID"), None);
    }
    fixture.finish().await;
}

#[tokio::test]
async fn succeeds_without_hooks() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .transaction(
            "project`.`project_transaction",
            "PHID-XACT-PROJ-1",
            "PHID-PROJ-build",
            "project:name",
            ("\"Old\"", "\"Build\""),
            "{}",
        )
        .await;
    fixture
        .story(1, "PHID-PROJ-build", &["PHID-XACT-PROJ-1"], None)
        .await;
    fixture
        .transaction(
            "user`.`user_transaction",
            "PHID-XACT-USER-1",
            "PHID-USER-alice",
            "user.rename",
            ("\"al\"", "\"alice\""),
            "{}",
        )
        .await;
    fixture
        .story(2, "PHID-USER-alice", &["PHID-XACT-USER-1"], None)
        .await;
    for story in [1, 2] {
        let id = fixture.schedule(&json!({"key": key(story)}), 2000).await;
        let archived = fixture.run(Config::default(), id).await;
        assert_archived(&archived, "0");
        assert_eq!(fixture.other_tasks(id).await, Vec::<Row>::new());
    }
    fixture.finish().await;
}

/// `loadFeedStory()` throws `PhabricatorWorkerPermanentFailureException`
/// when `PhabricatorFeedQuery` returns nothing, whatever the hooks.
#[tokio::test]
async fn fails_permanently_when_the_story_does_not_load() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // 1: hidden from feed (a self-subscription). 2: a transaction is
    // missing. 3: a project whose parent is gone. 4: only a silenced
    // project edge. 5: disabling a user. 6: no reference row. 7: not a
    // feed story class. 8: no transactions. 10: the key matches no story.
    fixture
        .transaction(
            "differential`.`differential_transaction",
            "PHID-XACT-DREV-self",
            "PHID-DREV-one",
            "core:subscribers",
            ("[]", "[\"PHID-USER-alice\"]"),
            "{}",
        )
        .await;
    fixture
        .story(1, "PHID-DREV-one", &["PHID-XACT-DREV-self"], None)
        .await;
    fixture
        .story(
            2,
            "PHID-DREV-one",
            &["PHID-XACT-DREV-self", "PHID-XACT-DREV-missing"],
            None,
        )
        .await;
    fixture
        .transaction(
            "project`.`project_transaction",
            "PHID-XACT-PROJ-orphan",
            "PHID-PROJ-orphan",
            "project:name",
            ("\"a\"", "\"b\""),
            "{}",
        )
        .await;
    fixture
        .story(3, "PHID-PROJ-orphan", &["PHID-XACT-PROJ-orphan"], None)
        .await;
    fixture
        .transaction(
            "project`.`project_transaction",
            "PHID-XACT-PROJ-silence",
            "PHID-PROJ-build",
            "core:edge",
            ("[]", "[\"PHID-USER-alice\"]"),
            "{\"edge:type\":61}",
        )
        .await;
    fixture
        .story(4, "PHID-PROJ-build", &["PHID-XACT-PROJ-silence"], None)
        .await;
    fixture
        .transaction(
            "user`.`user_transaction",
            "PHID-XACT-USER-disable",
            "PHID-USER-alice",
            "user.disable",
            ("false", "true"),
            "{}",
        )
        .await;
    fixture
        .story(5, "PHID-USER-alice", &["PHID-XACT-USER-disable"], None)
        .await;
    fixture
        .test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_feed`.feed_storydata (id, phid, chronologicalKey, storyType, \
               storyData, authorPHID, dateCreated, dateModified) VALUES \
             (6, 'PHID-STRY-6', {}, '{STORY}', '{{}}', 'PHID-USER-alice', 1, 1), \
             (7, 'PHID-STRY-7', {key}, 'PhabricatorFeedStoryDifferential', '{{}}', \
               'PHID-USER-alice', 1, 1); \
             INSERT INTO `{{ns}}_feed`.feed_storyreference (objectPHID, chronologicalKey) \
             VALUES ('PHID-DREV-one', {key})",
            key(6),
            key = key(7)
        ))
        .await;
    fixture
        .story(
            8,
            "PHID-DREV-one",
            &[],
            Some("{\"objectPHID\":\"PHID-DREV-one\",\"transactionPHIDs\":[]}"),
        )
        .await;

    let hooks = hooks(&["http://hooks.example/a"]);
    for story in [1, 2, 3, 4, 5, 6, 7, 8, 10] {
        let id = fixture.schedule(&json!({"key": key(story)}), 2000).await;
        let archived = fixture.run(hooks.clone(), id).await;
        assert_archived(&archived, "1");
        assert_eq!(
            fixture.other_tasks(id).await,
            Vec::<Row>::new(),
            "story {story}"
        );
    }
    fixture.finish().await;
}

/// `PhabricatorObjectQuery` skips the PHID types of uninstalled
/// applications, so their stories don't load.
#[tokio::test]
async fn uninstalled_applications_hide_their_stories() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .transaction(
            "project`.`project_transaction",
            "PHID-XACT-PROJ-name",
            "PHID-PROJ-build",
            "project:name",
            ("\"a\"", "\"b\""),
            "{}",
        )
        .await;
    fixture
        .story(9, "PHID-PROJ-build", &["PHID-XACT-PROJ-name"], None)
        .await;
    let mut uninstalled = Config::default();
    let mut layer = Map::new();
    layer.insert(
        "phabricator.uninstalled-applications".to_owned(),
        json!({"PhabricatorProjectApplication": true}),
    );
    uninstalled.push_layer("test", layer);
    let id = fixture.schedule(&json!({"key": key(9)}), 2000).await;
    assert_archived(&fixture.run(uninstalled, id).await, "1");
    let id = fixture.schedule(&json!({"key": key(9)}), 2000).await;
    assert_archived(&fixture.run(Config::default(), id).await, "0");
    fixture.finish().await;
}

/// Stories Rust can't load like PHP are never leased: the row stays as
/// PHP's lease query expects it, and the worker runs the next task.
#[tokio::test]
async fn leaves_unported_stories_to_php() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .story(1, "PHID-TASK-one", &["PHID-XACT-TASK-1"], None)
        .await;
    // A subscriber transaction PHP's handle code throws on.
    fixture
        .transaction(
            "differential`.`differential_transaction",
            "PHID-XACT-DREV-bad",
            "PHID-DREV-one",
            "core:subscribers",
            ("null", "[\"PHID-USER-bob\"]"),
            "{}",
        )
        .await;
    fixture
        .story(2, "PHID-DREV-one", &["PHID-XACT-DREV-bad"], None)
        .await;
    fixture
        .transaction(
            "differential`.`differential_transaction",
            "PHID-XACT-DREV-ok",
            "PHID-DREV-one",
            "differential.revision.accept",
            ("null", "true"),
            "{}",
        )
        .await;
    fixture
        .story(3, "PHID-DREV-one", &["PHID-XACT-DREV-ok"], None)
        .await;

    let task_story = fixture.schedule(&json!({"key": key(1)}), 1).await;
    let bad_story = fixture.schedule(&json!({"key": key(2)}), 1).await;
    let no_key = fixture.schedule(&json!({"chronologicalKey": 1}), 1).await;
    let expired = fixture.schedule(&json!({"key": key(1)}), 1).await;
    fixture
        .test_db
        .execute(&format!(
            "UPDATE `{{ns}}_worker`.worker_activetask SET leaseOwner = '1:2:php:1', \
               leaseExpires = 1000, failureCount = 2, failureTime = 900 WHERE id = {expired}"
        ))
        .await;
    let ours = fixture.schedule(&json!({"key": key(3)}), 2000).await;
    let declined = [task_story, bad_story, no_key, expired];
    let mut before = Vec::new();
    for id in declined {
        before.push(fixture.task_row("worker_activetask", id).await.unwrap());
    }

    let worker = fixture.worker(Config::default());
    let ran = worker.work_on(None).await.unwrap().unwrap();
    assert_eq!(ran.id, ours);
    assert!(matches!(ran.outcome, Ok(Executed::Succeeded(_))));
    assert!(worker.work_on(None).await.unwrap().is_none());
    for (id, row) in declined.iter().zip(before) {
        assert_eq!(
            fixture.task_row("worker_activetask", *id).await.unwrap(),
            row,
            "task {id}"
        );
        assert!(worker.declined.reason(*id).is_some(), "task {id}");
    }
    assert_eq!(
        worker.declined.reason(task_story).as_deref(),
        Some("feed story about an object type which is not ported")
    );
    fixture.finish().await;
}

/// A task leased before its story turned unported (here, leased without
/// inspection) is released with the lease it had: PHP's lease query finds
/// the row it would have found.
#[tokio::test]
async fn releases_tasks_declined_after_the_lease() {
    let Some(fixture) = fixture().await else {
        return;
    };
    fixture
        .story(1, "PHID-TASK-one", &["PHID-XACT-TASK-1"], None)
        .await;
    let fresh = fixture.schedule(&json!({"key": key(1)}), 2000).await;
    let expired = fixture.schedule(&json!({"key": key(1)}), 2000).await;
    fixture
        .test_db
        .execute(&format!(
            "UPDATE `{{ns}}_worker`.worker_activetask SET leaseOwner = '(yield)', \
               leaseExpires = 1000, dateModified = 1234 WHERE id = {expired}"
        ))
        .await;
    let owner = LeaseOwner::new("rust-test");
    for id in [fresh, expired] {
        let before = fixture.task_row("worker_activetask", id).await.unwrap();
        let task = LeaseQuery {
            classes: &[CLASS],
            ids: Some(&[id]),
            object_phids: None,
            limit: 1,
            inspection: None,
        }
        .execute(fixture.database(), &owner)
        .await
        .unwrap()
        .pop()
        .unwrap();
        assert_ne!(
            fixture.task_row("worker_activetask", id).await.unwrap(),
            before
        );
        let outcome = execute(
            task,
            &FeedPublisherWorker,
            fixture.database(),
            &Config::default(),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Executed::Declined { .. }), "{outcome:?}");
        assert_eq!(
            fixture.task_row("worker_activetask", id).await.unwrap(),
            before,
            "task {id}"
        );
        assert!(fixture.task_row("worker_archivetask", id).await.is_none());
    }
    fixture.finish().await;
}
