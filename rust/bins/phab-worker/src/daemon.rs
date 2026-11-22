//! `PhabricatorTaskmasterDaemon`: lease one task, run it, repeat; sleep a
//! second when the queue has nothing for us.

use std::time::Duration;

use phab_core::Config;
use phab_db::{Database, DbError};
use tokio::sync::watch;

use crate::execute::{ExecuteError, Executed, execute};
use crate::handler::Registry;
use crate::lease::{Declined, Inspection, LeaseOwner, LeaseQuery};

/// How long an idle taskmaster sleeps before looking again.
const IDLE_SLEEP: Duration = Duration::from_secs(1);

/// A taskmaster for the classes of its registry.
pub struct Worker {
    /// The databases.
    pub database: Database,
    /// The configuration.
    pub config: Config,
    /// The classes to run.
    pub registry: Registry,
    /// Names the leases.
    pub owner: LeaseOwner,
    /// The tasks the handlers left to PHP recently.
    pub declined: Declined,
}

/// A task a taskmaster leased and ran, and how that went.
#[derive(Debug)]
pub struct Ran {
    /// The task ID.
    pub id: i64,
    /// The task class.
    pub class: String,
    /// What `executeTask()` did, or the exception it let escape.
    pub outcome: Result<Executed, ExecuteError>,
}

impl Worker {
    /// A taskmaster which has declined nothing yet.
    pub fn new(database: Database, config: Config, registry: Registry, owner: LeaseOwner) -> Self {
        Self {
            database,
            config,
            registry,
            owner,
            declined: Declined::default(),
        }
    }

    /// One iteration: leases at most one task (`setLimit(1)`) and runs it.
    /// Returns whether there was a task.
    ///
    /// PHP's taskmaster exits when a task fails (the overseer restarts it);
    /// this logs the failure and carries on, which leaves the same rows.
    pub async fn work_once(&self) -> Result<bool, DbError> {
        Ok(self.work_on(None).await?.is_some())
    }

    /// Leases at most one task, among `ids` if given, and runs it; `None`
    /// when there was none to lease. Tasks whose handlers decline them
    /// stay in the queue, recorded in [`Worker::declined`].
    pub async fn work_on(&self, ids: Option<&[i64]>) -> Result<Option<Ran>, DbError> {
        let classes = self.registry.classes();
        let tasks = LeaseQuery {
            classes: &classes,
            ids,
            object_phids: None,
            limit: 1,
            inspection: Some(Inspection {
                registry: &self.registry,
                database: &self.database,
                config: &self.config,
                declined: &self.declined,
            }),
        }
        .execute(&self.database, &self.owner)
        .await?;
        let Some(task) = tasks.into_iter().next() else {
            return Ok(None);
        };
        let id = task.id;
        let class = task.task_class.clone();
        let Some(handler) = self.registry.get(&class) else {
            // The lease query only takes registered classes.
            tracing::error!(id, class, "leased a task without a handler");
            return Ok(None);
        };
        tracing::info!(id, class, "Working on task {id} ({class})...");
        let outcome = execute(task, handler.as_ref(), &self.database, &self.config).await;
        match &outcome {
            Ok(Executed::Succeeded(_)) => {
                tracing::info!(id, "Task {id} complete! Moved to archive.");
            }
            Ok(Executed::FailedPermanently { message, .. }) => {
                tracing::error!(
                    id,
                    message,
                    "Task \"{id}\" encountered a permanent failure and was cancelled."
                );
            }
            Ok(Executed::Yielded(_)) => tracing::info!(id, "Task {id} yielded."),
            Ok(Executed::Failed { task, message }) => {
                tracing::error!(
                    id,
                    message,
                    failures = task.failure_count,
                    "Task {id} failed!"
                );
            }
            Ok(Executed::Declined { reason, .. }) => {
                tracing::info!(id, reason, "Task {id} ({class}) left to PHP: {reason}");
                self.declined.insert(id, reason.clone());
            }
            Err(error) => {
                tracing::error!(id, %error, "Error while executing Task ID {id}.");
            }
        }
        Ok(Some(Ran { id, class, outcome }))
    }

    /// Works until `shutdown` turns true, finishing the task at hand first.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        while !*shutdown.borrow() {
            let idle = match self.work_once().await {
                Ok(found) => !found,
                Err(error) => {
                    tracing::error!(%error, "cannot lease tasks");
                    true
                }
            };
            if idle {
                tokio::select! {
                    () = tokio::time::sleep(IDLE_SLEEP) => {}
                    _ = shutdown.changed() => {}
                }
            }
        }
    }
}
