//! `PhabricatorWorkerActiveTask` and `PhabricatorWorkerArchiveTask`: the
//! rows of `worker_activetask` and `worker_archivetask`.

use phab_core::php;
use phab_db::{Database, DbError};
use serde_json::Value;
use thiserror::Error;

use crate::{YIELD_OWNER, time};

/// `PhabricatorWorkerArchiveTask::RESULT_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskResult {
    /// `RESULT_SUCCESS`.
    Success,
    /// `RESULT_FAILURE`: a permanent failure.
    Failure,
    /// `RESULT_CANCELLED`, written by `bin/worker cancel`.
    Cancelled,
}

impl TaskResult {
    /// The value of the `result` column.
    pub fn code(self) -> i64 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Cancelled => 2,
        }
    }
}

/// `checkLease()` refused a write: the lease ran out while the task ran.
#[derive(Debug, Error)]
#[error("Trying to update Task {id} ({class}) after lease expiration!")]
pub struct LeaseExpired {
    /// The task ID.
    pub id: i64,
    /// The task class.
    pub class: String,
}

/// Why a task row could not be written.
#[derive(Debug, Error)]
pub enum WriteError {
    /// The lease expired; PHP throws before writing.
    #[error(transparent)]
    LeaseExpired(#[from] LeaseExpired),
    /// The query failed.
    #[error(transparent)]
    Database(#[from] DbError),
}

impl From<sqlx::Error> for WriteError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error.into())
    }
}

/// A leased row of `worker_activetask`, with its `worker_taskdata`.
#[derive(Debug, Clone)]
pub struct ActiveTask {
    /// `id`.
    pub id: i64,
    /// `taskClass`.
    pub task_class: String,
    /// `leaseOwner`.
    pub lease_owner: Option<String>,
    /// `leaseExpires`, in server time.
    pub lease_expires: Option<i64>,
    /// `failureCount`.
    pub failure_count: i64,
    /// `dataID`.
    pub data_id: i64,
    /// `failureTime`.
    pub failure_time: Option<i64>,
    /// `priority`.
    pub priority: i64,
    /// `objectPHID`.
    pub object_phid: Option<String>,
    /// `containerPHID`.
    pub container_phid: Option<String>,
    /// `dateCreated`.
    pub date_created: i64,
    /// `dateModified`.
    pub date_modified: i64,
    /// The task data, `json_decode($data, true)`; `null` when the row has
    /// none or it is not JSON.
    pub data: Value,
    /// `UNIX_TIMESTAMP()` when the task was loaded (`setServerTime()`).
    pub server_time: i64,
    /// `time()` when the task was loaded.
    pub local_time: i64,
    /// The lease the row had before the lease query took it, put back by
    /// [`ActiveTask::release`].
    pub prior_lease: PriorLease,
}

/// The columns of a task row the worker changes before `doWork()`, as they
/// were before it leased the task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorLease {
    /// `leaseOwner`.
    pub owner: Option<String>,
    /// `leaseExpires`.
    pub expires: Option<i64>,
    /// `dateModified`, which only `setLeaseDuration()` changes.
    pub date_modified: i64,
}

/// A row written to `worker_archivetask`.
#[derive(Debug, Clone)]
pub struct ArchiveTask {
    /// The task, as it was when archived.
    pub task: ActiveTask,
    /// `result`.
    pub result: TaskResult,
    /// `duration`, in microseconds.
    pub duration: i64,
    /// `archivedEpoch`.
    pub archived_epoch: i64,
}

/// `if ($row['_taskData']) json_decode(..., true) else null`.
pub(crate) fn decode_data(raw: Option<&str>) -> Value {
    match raw {
        Some(text) if php::is_truthy_str(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

impl ActiveTask {
    /// `checkLease()`: fails once the lease has run out, measured on the
    /// server's clock as of the load plus the local time since then.
    pub fn check_lease(&self) -> Result<(), LeaseExpired> {
        let Some(owner) = self.lease_owner.as_deref() else {
            return Ok(());
        };
        if !php::is_truthy_str(owner) || owner == YIELD_OWNER {
            return Ok(());
        }
        let current_server_time = self.server_time + (time() - self.local_time);
        // `$time >= null` is true in PHP: a task without an expiry has none
        // left.
        if self
            .lease_expires
            .is_none_or(|expires| current_server_time >= expires)
        {
            return Err(LeaseExpired {
                id: self.id,
                class: self.task_class.clone(),
            });
        }
        Ok(())
    }

    /// `setLeaseDuration()`: checks the lease, then moves its expiry to
    /// `duration` seconds after the load (not after now) and saves.
    pub async fn set_lease_duration(
        &mut self,
        database: &Database,
        duration: i64,
    ) -> Result<(), WriteError> {
        self.check_lease()?;
        self.lease_expires = Some(self.server_time + duration);
        self.force_save_without_lease(database).await?;
        Ok(())
    }

    /// `forceSaveWithoutLease()` of a saved task: `LiskDAO::update()`,
    /// which writes every column and sets `dateModified`.
    pub async fn force_save_without_lease(&mut self, database: &Database) -> Result<(), DbError> {
        self.date_modified = time();
        sqlx::query(&format!(
            "UPDATE {} SET taskClass = ?, leaseOwner = ?, leaseExpires = ?, failureCount = ?, \
             dataID = ?, failureTime = ?, priority = ?, objectPHID = ?, containerPHID = ?, \
             dateCreated = ?, dateModified = ? WHERE id = ?",
            database.table("worker", "worker_activetask")
        ))
        .bind(&self.task_class)
        .bind(&self.lease_owner)
        .bind(self.lease_expires)
        .bind(self.failure_count)
        .bind(self.data_id)
        .bind(self.failure_time)
        .bind(self.priority)
        .bind(&self.object_phid)
        .bind(&self.container_phid)
        .bind(self.date_created)
        .bind(self.date_modified)
        .bind(self.id)
        .execute(database.pool())
        .await?;
        Ok(())
    }

    /// Hands a declined task back: puts back the lease columns as the lease
    /// query found them, if this lease still holds the row, so PHP's lease
    /// query sees the row it would have seen without this worker (an
    /// unleased task is taken by its next `PHASE_UNLEASED` query, a task
    /// whose lease had expired by its `PHASE_EXPIRED` one, in the same
    /// order). Nothing else of the row was written.
    pub async fn release(&self, database: &Database) -> Result<(), DbError> {
        let Some(owner) = self.lease_owner.as_deref() else {
            return Ok(());
        };
        sqlx::query(&format!(
            "UPDATE {} SET leaseOwner = ?, leaseExpires = ?, dateModified = ? \
             WHERE id = ? AND leaseOwner = ?",
            database.table("worker", "worker_activetask")
        ))
        .bind(&self.prior_lease.owner)
        .bind(self.prior_lease.expires)
        .bind(self.prior_lease.date_modified)
        .bind(self.id)
        .bind(owner)
        .execute(database.pool())
        .await?;
        Ok(())
    }

    /// `archiveTask()`: checks the lease, then, in one transaction, deletes
    /// the active row and inserts the archived one
    /// (`PhabricatorWorkerArchiveTask::save()`). The data row is kept.
    pub async fn archive(
        &self,
        database: &Database,
        result: TaskResult,
        duration: i64,
    ) -> Result<ArchiveTask, WriteError> {
        self.check_lease()?;
        let now = time();
        let mut transaction = database.pool().begin().await?;
        sqlx::query(&format!(
            "DELETE FROM {} WHERE id = ?",
            database.table("worker", "worker_activetask")
        ))
        .bind(self.id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(&format!(
            "INSERT INTO {} (id, taskClass, leaseOwner, leaseExpires, failureCount, dataID, \
             priority, objectPHID, containerPHID, result, duration, dateCreated, dateModified, \
             archivedEpoch) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            database.table("worker", "worker_archivetask")
        ))
        .bind(self.id)
        .bind(&self.task_class)
        .bind(&self.lease_owner)
        .bind(self.lease_expires)
        .bind(self.failure_count)
        .bind(self.data_id)
        .bind(self.priority)
        .bind(&self.object_phid)
        .bind(&self.container_phid)
        .bind(result.code())
        .bind(duration)
        .bind(self.date_created)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        let mut task = self.clone();
        task.date_modified = now;
        Ok(ArchiveTask {
            task,
            result,
            duration,
            archived_epoch: now,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(owner: Option<&str>, expires: Option<i64>) -> ActiveTask {
        let now = time();
        ActiveTask {
            id: 1,
            task_class: "PhabricatorTestWorker".to_owned(),
            lease_owner: owner.map(str::to_owned),
            lease_expires: expires.map(|offset| now + offset),
            failure_count: 0,
            data_id: 1,
            failure_time: None,
            priority: 2000,
            object_phid: None,
            container_phid: None,
            date_created: now,
            date_modified: now,
            data: Value::Null,
            server_time: now,
            local_time: now,
            prior_lease: PriorLease::default(),
        }
    }

    #[test]
    fn checks_the_lease_like_php() {
        assert!(task(Some("1:2:host:1"), Some(60)).check_lease().is_ok());
        assert!(task(Some("1:2:host:1"), Some(0)).check_lease().is_err());
        assert!(task(Some("1:2:host:1"), None).check_lease().is_err());
        assert!(task(Some(YIELD_OWNER), Some(-60)).check_lease().is_ok());
        assert!(task(None, Some(-60)).check_lease().is_ok());
        assert!(task(Some("0"), Some(-60)).check_lease().is_ok());
        assert_eq!(
            task(Some("x"), Some(-1))
                .check_lease()
                .unwrap_err()
                .to_string(),
            "Trying to update Task 1 (PhabricatorTestWorker) after lease expiration!"
        );
    }

    #[test]
    fn decodes_task_data_like_php() {
        assert_eq!(decode_data(None), Value::Null);
        assert_eq!(decode_data(Some("")), Value::Null);
        assert_eq!(decode_data(Some("0")), Value::Null);
        assert_eq!(decode_data(Some("{")), Value::Null);
        assert_eq!(decode_data(Some("[]")), serde_json::json!([]));
        assert_eq!(decode_data(Some(r#"{"a":1}"#)), serde_json::json!({"a": 1}));
    }
}
