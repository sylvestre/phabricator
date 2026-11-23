//! `PhabricatorWorkerActiveTask::executeTask()`.

use phab_core::{Config, php};
use phab_db::worker::{Task, schedule_contained_task};
use phab_db::{Database, DbError};
use thiserror::Error;

use crate::handler::{Failure, Handler, QueuedTask, Work};
use crate::lease::DEFAULT_WAIT_BEFORE_RETRY;
use crate::task::{ActiveTask, ArchiveTask, LeaseExpired, TaskResult, WriteError};
use crate::{YIELD_OWNER, time};

/// The shortest yield, in seconds (`max($retry, 5)`).
const MINIMUM_YIELD: i64 = 5;

/// What running a task did to its row.
#[derive(Debug)]
pub enum Executed {
    /// Archived with `RESULT_SUCCESS`.
    Succeeded(ArchiveTask),
    /// Archived with `RESULT_FAILURE` (duration 0).
    FailedPermanently {
        /// The archived row.
        archive: ArchiveTask,
        /// The failure message.
        message: String,
    },
    /// Left active, owned by `(yield)`, until the yield runs out.
    Yielded(ActiveTask),
    /// Left active with one more failure, until the retry wait runs out.
    Failed {
        /// The task as saved.
        task: ActiveTask,
        /// The failure message.
        message: String,
    },
    /// Handed back to PHP with the lease it had before
    /// ([`Failure::Decline`]).
    Declined {
        /// The task as released.
        task: ActiveTask,
        /// Why.
        reason: String,
    },
}

/// An exception PHP lets escape `executeTask()`: the daemon dies and the
/// task keeps its row as it was last saved, until its lease expires.
#[derive(Debug, Error)]
pub enum ExecuteError {
    /// The lease ran out before the result could be written.
    #[error(transparent)]
    LeaseExpired(#[from] LeaseExpired),
    /// A query failed outside the part PHP retries.
    #[error(transparent)]
    Database(#[from] DbError),
    /// `getWaitBeforeRetry()` failed.
    #[error("getWaitBeforeRetry() failed: {0}")]
    Hook(Failure),
}

impl From<WriteError> for ExecuteError {
    fn from(error: WriteError) -> Self {
        match error {
            WriteError::LeaseExpired(error) => Self::LeaseExpired(error),
            WriteError::Database(error) => Self::Database(error),
        }
    }
}

/// What the `try` block of `executeTask()` throws, by the `catch` that
/// takes it.
enum Caught {
    Permanent(String),
    Yield(i64),
    Other(String),
    Decline(String),
}

impl From<Failure> for Caught {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Permanent(message) => Self::Permanent(message),
            Failure::Yield(duration) => Self::Yield(duration),
            Failure::Temporary(message) => Self::Other(message),
            Failure::Decline(reason) => Self::Decline(reason),
        }
    }
}

impl From<WriteError> for Caught {
    fn from(error: WriteError) -> Self {
        Self::Other(error.to_string())
    }
}

/// The `try` block: retry limit, lease time, `doWork()`, archive.
async fn attempt(
    task: &mut ActiveTask,
    handler: &dyn Handler,
    database: &Database,
    config: &Config,
) -> Result<(ArchiveTask, Vec<QueuedTask>), Caught> {
    if let Some(maximum) = handler.maximum_retry_count(task)?
        && task.failure_count > maximum
    {
        return Err(Caught::Permanent(format!(
            "Task {} has exceeded the maximum number of failures ({maximum}).",
            task.id
        )));
    }
    if let Some(lease) = handler.required_lease_time(task)? {
        task.set_lease_duration(database, lease).await?;
    }
    let start = std::time::Instant::now();
    let work = Work::new(task, database, config);
    handler.do_work(&work).await?;
    let queued = work.into_queued();
    let duration = i64::try_from(start.elapsed().as_micros()).unwrap_or(i64::MAX);
    let archive = task
        .archive(database, TaskResult::Success, duration)
        .await?;
    Ok((archive, queued))
}

/// `flushTaskQueue()` with the task's priority as the default.
async fn flush(
    database: &Database,
    queued: Vec<QueuedTask>,
    default_priority: i64,
) -> Result<(), DbError> {
    for followup in queued {
        let data = php::json_encode(&followup.data);
        let mut connection = database.pool().acquire().await?;
        schedule_contained_task(
            &mut connection,
            database,
            Task {
                class: &followup.class,
                data: &data,
                priority: followup.priority.unwrap_or(default_priority),
                object_phid: followup.object_phid.as_deref(),
            },
            followup.container_phid.as_deref(),
            time(),
        )
        .await?;
    }
    Ok(())
}

/// Runs a leased `task` with its `handler` and writes the outcome as PHP
/// does.
pub async fn execute(
    mut task: ActiveTask,
    handler: &dyn Handler,
    database: &Database,
    config: &Config,
) -> Result<Executed, ExecuteError> {
    // Outside the `try`: without the lease, the task is not ours to touch.
    task.check_lease()?;
    match attempt(&mut task, handler, database, config).await {
        Ok((archive, queued)) => {
            // After the archive, so a failure here doesn't fail the task.
            flush(database, queued, task.priority).await?;
            Ok(Executed::Succeeded(archive))
        }
        Err(Caught::Permanent(message)) => {
            let archive = task.archive(database, TaskResult::Failure, 0).await?;
            Ok(Executed::FailedPermanently { archive, message })
        }
        Err(Caught::Yield(duration)) => {
            task.lease_owner = Some(YIELD_OWNER.to_owned());
            task.set_lease_duration(database, duration.max(MINIMUM_YIELD))
                .await?;
            Ok(Executed::Yielded(task))
        }
        Err(Caught::Decline(reason)) => {
            // Before anything of PHP's `catch` blocks: the row goes back to
            // how the lease query found it.
            task.release(database).await?;
            Ok(Executed::Declined { task, reason })
        }
        Err(Caught::Other(message)) => {
            task.failure_count += 1;
            task.failure_time = Some(time());
            let retry = handler
                .wait_before_retry(&task)
                .map_err(ExecuteError::Hook)?
                .unwrap_or(DEFAULT_WAIT_BEFORE_RETRY);
            task.set_lease_duration(database, retry).await?;
            Ok(Executed::Failed { task, message })
        }
    }
}
