//! `PhabricatorTaskmasterDaemon`: lease one task, run it, repeat; sleep a
//! second when the queue has nothing for us.

use std::time::Duration;

use phab_core::Config;
use phab_db::{Database, DbError};
use tokio::sync::watch;

use crate::execute::{Executed, execute};
use crate::handler::Registry;
use crate::lease::{LeaseOwner, LeaseQuery};

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
}

impl Worker {
    /// One iteration: leases at most one task (`setLimit(1)`) and runs it.
    /// Returns whether there was a task.
    ///
    /// PHP's taskmaster exits when a task fails (the overseer restarts it);
    /// this logs the failure and carries on, which leaves the same rows.
    pub async fn work_once(&self) -> Result<bool, DbError> {
        let classes = self.registry.classes();
        let tasks = LeaseQuery {
            classes: &classes,
            ids: None,
            object_phids: None,
            limit: 1,
        }
        .execute(&self.database, &self.owner)
        .await?;
        let found = !tasks.is_empty();
        for task in tasks {
            let id = task.id;
            let class = task.task_class.clone();
            let Some(handler) = self.registry.get(&class) else {
                // The lease query only takes registered classes.
                tracing::error!(id, class, "leased a task without a handler");
                continue;
            };
            tracing::info!(id, class, "Working on task {id} ({class})...");
            match execute(task, handler.as_ref(), &self.database, &self.config).await {
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
                Err(error) => {
                    tracing::error!(id, %error, "Error while executing Task ID {id}.");
                }
            }
        }
        Ok(found)
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
