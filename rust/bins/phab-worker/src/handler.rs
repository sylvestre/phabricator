//! `PhabricatorWorker`: the Rust port of a task class.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use phab_core::Config;
use phab_db::Database;
use serde_json::Value;

use crate::task::ActiveTask;

/// How a task stopped, when it did not succeed: the exception PHP's
/// `executeTask()` catches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// `PhabricatorWorkerPermanentFailureException`: archived with
    /// `RESULT_FAILURE`, never retried.
    Permanent(String),
    /// `PhabricatorWorkerYieldException`: tried again after this many
    /// seconds (at least 5), without counting a failure.
    Yield(i64),
    /// Any other exception: counted as a failure and retried after
    /// [`Handler::wait_before_retry`].
    Temporary(String),
    /// Not an outcome PHP has: the task needs code which is not ported, so
    /// the worker puts the lease back as it found it and PHP's taskmasters
    /// run the task (see [`Handler::inspect`]).
    Decline(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Permanent(message) | Self::Temporary(message) | Self::Decline(message) => {
                formatter.write_str(message)
            }
            Self::Yield(duration) => write!(formatter, "yielded for {duration} seconds"),
        }
    }
}

/// A follow-up task from `queueTask()`, scheduled only if the task
/// succeeds (`flushTaskQueue()`).
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedTask {
    /// The task class.
    pub class: String,
    /// The task data.
    pub data: Value,
    /// `priority`; the queuing task's priority when `None`.
    pub priority: Option<i64>,
    /// `objectPHID`.
    pub object_phid: Option<String>,
    /// `containerPHID`.
    pub container_phid: Option<String>,
}

/// What a handler sees while it runs a task.
pub struct Work<'a> {
    /// The task, as leased.
    pub task: &'a ActiveTask,
    /// The databases.
    pub database: &'a Database,
    /// The configuration.
    pub config: &'a Config,
    queued: Mutex<Vec<QueuedTask>>,
}

impl<'a> Work<'a> {
    /// The context for running `task`.
    pub fn new(task: &'a ActiveTask, database: &'a Database, config: &'a Config) -> Self {
        Self {
            task,
            database,
            config,
            queued: Mutex::new(Vec::new()),
        }
    }

    /// `getTaskData()`.
    pub fn data(&self) -> &Value {
        &self.task.data
    }

    /// `queueTask()`.
    pub fn queue_task(&self, task: QueuedTask) {
        self.queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
    }

    /// The tasks queued so far (`getQueuedTasks()`).
    pub fn into_queued(self) -> Vec<QueuedTask> {
        self.queued
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The future [`Handler::do_work`] returns.
pub type WorkFuture<'a> = Pin<Box<dyn Future<Output = Result<(), Failure>> + Send + 'a>>;

/// A task the lease query found, before it leases it.
pub struct Unleased<'a> {
    /// `id`.
    pub id: i64,
    /// `taskClass`.
    pub class: &'a str,
    /// The task data, decoded as [`ActiveTask::data`].
    pub data: &'a Value,
    /// The databases.
    pub database: &'a Database,
    /// The configuration.
    pub config: &'a Config,
}

/// Whether the worker leases a task it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// Lease and run it.
    Take,
    /// Leave it to PHP, for this reason.
    Decline(String),
}

/// The future [`Handler::inspect`] returns.
pub type ClaimFuture<'a> = Pin<Box<dyn Future<Output = Claim> + Send + 'a>>;

/// A task class ported to Rust (a subclass of `PhabricatorWorker`).
///
/// The hooks get the task as it is when PHP calls them:
/// [`Handler::wait_before_retry`] sees the failure already counted. An
/// error from `required_lease_time` or `maximum_retry_count` is handled
/// like an exception of `doWork()`; one from `wait_before_retry` escapes
/// `executeTask()` like PHP's does, so nothing is written.
pub trait Handler: Send + Sync {
    /// The PHP class name, the `taskClass` of its rows.
    fn class(&self) -> &'static str;

    /// `getRequiredLeaseTime()`: seconds of lease the task needs, or the
    /// lease taken by the query (2 hours).
    fn required_lease_time(&self, _task: &ActiveTask) -> Result<Option<i64>, Failure> {
        Ok(None)
    }

    /// `getMaximumRetryCount()`: failures allowed before the task fails
    /// permanently, or no limit.
    fn maximum_retry_count(&self, _task: &ActiveTask) -> Result<Option<i64>, Failure> {
        Ok(None)
    }

    /// `getWaitBeforeRetry()`: seconds to wait after a failure, or 5
    /// minutes.
    fn wait_before_retry(&self, _task: &ActiveTask) -> Result<Option<i64>, Failure> {
        Ok(None)
    }

    /// Whether to lease `task`, called before the lease: a class ported for
    /// only some of its tasks declines the others, which are then never
    /// leased, so their rows stay as they are and PHP's lease query takes
    /// them as if this worker didn't exist. What it reads may change before
    /// [`Handler::do_work`] runs, which can still decline with
    /// [`Failure::Decline`].
    fn inspect<'a>(&'a self, _task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(std::future::ready(Claim::Take))
    }

    /// `doWork()`.
    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a>;
}

/// The task classes this worker runs, by class name.
#[derive(Default, Clone)]
pub struct Registry {
    handlers: BTreeMap<&'static str, Arc<dyn Handler>>,
}

impl Registry {
    /// Adds `handler`, replacing one for the same class.
    pub fn register(&mut self, handler: impl Handler + 'static) {
        self.handlers.insert(handler.class(), Arc::new(handler));
    }

    /// The handler of `class`.
    pub fn get(&self, class: &str) -> Option<&Arc<dyn Handler>> {
        self.handlers.get(class)
    }

    /// The registered classes, sorted.
    pub fn classes(&self) -> Vec<&'static str> {
        self.handlers.keys().copied().collect()
    }

    /// Keeps only the classes in `allowed`; returns the names in `allowed`
    /// which have no handler.
    pub fn retain<'a>(&mut self, allowed: &[&'a str]) -> Vec<&'a str> {
        self.handlers.retain(|class, _| allowed.contains(class));
        allowed
            .iter()
            .copied()
            .filter(|class| !self.handlers.contains_key(class))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop(&'static str);

    impl Handler for Noop {
        fn class(&self) -> &'static str {
            self.0
        }

        fn do_work<'a>(&'a self, _work: &'a Work<'a>) -> WorkFuture<'a> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn restricts_the_registered_classes() {
        let mut registry = Registry::default();
        registry.register(Noop("B"));
        registry.register(Noop("A"));
        assert_eq!(registry.classes(), ["A", "B"]);
        assert_eq!(registry.retain(&["B", "C"]), ["C"]);
        assert_eq!(registry.classes(), ["B"]);
        assert!(registry.get("A").is_none());
    }
}
