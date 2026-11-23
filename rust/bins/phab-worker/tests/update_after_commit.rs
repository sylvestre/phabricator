//! `DiffusionUpdateObjectAfterCommitWorker` against a throwaway `MySQL`
//! database: the tasks PHP fails for good, and the closing of revisions,
//! left to PHP.

use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_db::worker::{Task, schedule_task};
use phab_worker::handlers::update_after_commit::UpdateObjectAfterCommitWorker;
use phab_worker::{Executed, LeaseOwner, Registry, Worker};
use serde_json::{Value, json};

const DATABASES: &[&str] = &["worker", "repository", "differential"];

/// Commit 1 of repository 1; revision 1.
async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(DATABASES).await?;
    test_db
        .execute(
            "INSERT INTO `{ns}_repository`.repository (id, phid, name, callsign, \
               versionControlSystem, details, dateCreated, dateModified, viewPolicy, editPolicy, \
               pushPolicy) VALUES \
             (1, 'PHID-REPO-git', 'Git', NULL, 'git', '{}', 1, 1, 'users', 'admin', 'admin'); \
             INSERT INTO `{ns}_repository`.repository_commit (id, repositoryID, phid, \
               commitIdentifier, epoch, auditStatus, summary, importStatus) VALUES \
             (1, 1, 'PHID-CMIT-1', 'aaaaaaa111', 1, 'none', 'S1', 15); \
             INSERT INTO `{ns}_differential`.differential_revision (id, title, phid, status, \
               summary, testPlan, authorPHID, dateCreated, dateModified, attached, mailKey, \
               viewPolicy, editPolicy, repositoryPHID, properties, activeDiffPHID) VALUES \
             (1, 'Ships', 'PHID-DREV-1', 'accepted', '', '', 'PHID-USER-alice', 1, 1, '{}', \
               'k', 'users', 'users', 'PHID-REPO-git', '{}', 'PHID-DIFF-1')",
        )
        .await;
    Some(test_db)
}

async fn schedule(test_db: &TestDatabase, data: &Value) -> i64 {
    let data = phab_core::php::json_encode(data);
    let database = test_db.database();
    let mut connection = database.pool().acquire().await.unwrap();
    let task = Task {
        class: "DiffusionUpdateObjectAfterCommitWorker",
        data: &data,
        priority: 2000,
        object_phid: None,
    };
    let id = schedule_task(&mut connection, database, task, 1_600_000_000)
        .await
        .unwrap();
    i64::try_from(id).unwrap()
}

/// Runs task `id`; `None` when the handler left it to PHP.
async fn run(test_db: &TestDatabase, id: i64) -> Option<Executed> {
    let mut registry = Registry::default();
    registry.register(UpdateObjectAfterCommitWorker);
    let worker = Worker::new(
        test_db.database().clone(),
        Config::default(),
        registry,
        LeaseOwner::new("rust-test"),
    );
    let ran = worker.work_on(Some(&[id])).await.unwrap()?;
    assert_eq!(ran.id, id);
    Some(ran.outcome.unwrap())
}

#[tokio::test]
async fn fails_as_php_does() {
    let Some(test_db) = setup().await else {
        return;
    };
    for (data, expected) in [
        (json!({}), "No \"commitPHID\" in task data."),
        (
            json!({"commitPHID": "PHID-CMIT-9"}),
            "Unable to load commit \"PHID-CMIT-9\".",
        ),
        (
            json!({"commitPHID": "PHID-CMIT-1", "objectPHID": ""}),
            "No \"objectPHID\" in task data.",
        ),
        (
            json!({"commitPHID": "PHID-CMIT-1", "objectPHID": "PHID-DREV-9"}),
            "Unable to load object \"PHID-DREV-9\".",
        ),
    ] {
        let id = schedule(&test_db, &data).await;
        match run(&test_db, id).await {
            Some(Executed::FailedPermanently { message, .. }) => assert_eq!(message, expected),
            other => panic!("{data}: {other:?}"),
        }
    }
    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_closing_revisions_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let data = json!({
        "commitPHID": "PHID-CMIT-1",
        "objectPHID": "PHID-DREV-1",
        "properties": {"revisionMatchData": null},
    });
    let id = schedule(&test_db, &data).await;
    assert!(run(&test_db, id).await.is_none());
    test_db.drop_databases().await;
}
