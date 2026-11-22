//! `PhabricatorWorker::scheduleTask()`: queues a task for the PHP daemons,
//! so work Rust does not do yet (publishing mail and feed stories, search
//! indexing) still happens.

use sqlx::MySqlConnection;

use crate::{Database, DbError};

/// `PhabricatorWorker::PRIORITY_ALERTS`.
pub const PRIORITY_ALERTS: i64 = 1000;
/// `PhabricatorWorker::PRIORITY_DEFAULT`, for tasks queued without one.
pub const PRIORITY_DEFAULT: i64 = 2000;

/// A task to queue.
#[derive(Debug, Clone, Copy)]
pub struct Task<'a> {
    /// The worker class, like `PhabricatorApplicationTransactionPublishWorker`.
    pub class: &'a str,
    /// The task data, as PHP's `json_encode()` would write it.
    pub data: &'a str,
    /// The priority.
    pub priority: i64,
    /// The object the task is about.
    pub object_phid: Option<&'a str>,
}

/// Queues `task` on `connection`, as `PhabricatorWorkerActiveTask::save()`
/// does: the data row first, then an ID from the `worker_activetask`
/// counter (`LiskDAO::IDS_COUNTER`). Returns the task ID.
pub async fn schedule_task(
    connection: &mut MySqlConnection,
    database: &Database,
    task: Task<'_>,
    now: i64,
) -> Result<u64, DbError> {
    schedule_contained_task(connection, database, task, None, now).await
}

/// [`schedule_task()`] with a `containerPHID`: the object containing the
/// one the task is about, like the buildable of a build.
pub async fn schedule_contained_task(
    connection: &mut MySqlConnection,
    database: &Database,
    task: Task<'_>,
    container_phid: Option<&str>,
    now: i64,
) -> Result<u64, DbError> {
    let data_id = sqlx::query(&format!(
        "INSERT INTO {} (data) VALUES (?)",
        database.table("worker", "worker_taskdata")
    ))
    .bind(task.data)
    .execute(&mut *connection)
    .await?
    .last_insert_id();
    let id = sqlx::query(&format!(
        "INSERT INTO {} (counterName, counterValue) VALUES ('worker_activetask', \
         LAST_INSERT_ID(1)) ON DUPLICATE KEY UPDATE counterValue = LAST_INSERT_ID(counterValue + 1)",
        database.table("worker", "lisk_counter")
    ))
    .execute(&mut *connection)
    .await?
    .last_insert_id();
    sqlx::query(&format!(
        "INSERT INTO {} (id, taskClass, leaseOwner, leaseExpires, failureCount, dataID, \
         failureTime, priority, objectPHID, dateCreated, dateModified, containerPHID) VALUES \
         (?, ?, NULL, NULL, 0, ?, NULL, ?, ?, ?, ?, ?)",
        database.table("worker", "worker_activetask")
    ))
    .bind(id)
    .bind(task.class)
    .bind(data_id)
    .bind(task.priority)
    .bind(task.object_phid)
    .bind(now)
    .bind(now)
    .bind(container_phid)
    .execute(&mut *connection)
    .await?;
    Ok(id)
}
