//! `PhabricatorApplicationTransactionPublishWorker`, which editors queue
//! once they applied transactions, with the object, the acting user, the
//! transactions and the editor's state as its data: it queues the search
//! indexing task, publishes the feed story and saves the mail
//! ([`phab_app_transactions::publish`]).
//!
//! Ported for Differential revisions; tasks about other objects, and those
//! whose mail or story Rust can't build as PHP does, are declined before
//! the lease.

use phab_app_transactions::Error;
use phab_app_transactions::publish::{self, Plan};
use phab_core::Config;
use phab_db::Database;
use serde_json::Value;

use crate::handler::{Claim, ClaimFuture, Failure, Handler, Unleased, Work, WorkFuture};

/// `PhabricatorApplicationTransactionPublishWorker`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PublishWorker;

/// Plans the task.
async fn plan(data: &Value, database: &Database, config: &Config) -> Result<Plan, Failure> {
    match publish::plan(database, config, data, crate::time()).await {
        Ok(plan) => Ok(plan),
        Err(Error::Unsupported(reason)) => Err(Failure::Decline(reason)),
        Err(Error::Db(error)) => Err(Failure::Temporary(error.to_string())),
    }
}

impl Handler for PublishWorker {
    fn class(&self) -> &'static str {
        "PhabricatorApplicationTransactionPublishWorker"
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(async move {
            match plan(task.data, task.database, task.config).await {
                Ok(_) => Claim::Take,
                // A query failing now is left to PHP too.
                Err(failure) => Claim::Decline(failure.to_string()),
            }
        })
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            match plan(work.data(), work.database, work.config).await? {
                Plan::Fail(message) => Err(Failure::Permanent(message)),
                Plan::Publish(publication) => {
                    publish::write(work.database, &publication, crate::time())
                        .await
                        .map_err(|error| Failure::Temporary(error.to_string()))
                }
            }
        })
    }
}
