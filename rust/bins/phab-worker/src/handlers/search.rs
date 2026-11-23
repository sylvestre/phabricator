//! `PhabricatorSearchWorker`, which editors queue
//! (`queueDocumentForIndexing()`) once an object changed: it rebuilds the
//! object's search indexes ([`phab_app_search`]).
//!
//! Ported for Differential revisions, users and projects without parent,
//! subprojects or milestones; other objects, and those whose text or
//! configuration Rust can't index as PHP does, are declined before the
//! lease.

use phab_app_search::{Error, Outcome};

use crate::handler::{Claim, ClaimFuture, Failure, Handler, Unleased, Work, WorkFuture};

/// `PhabricatorSearchWorker`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchWorker;

impl Handler for SearchWorker {
    fn class(&self) -> &'static str {
        "PhabricatorSearchWorker"
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(async move {
            match phab_app_search::inspect(task.database, task.config, task.data).await {
                Ok(()) => Claim::Take,
                // A query failing now is left to PHP too.
                Err(error) => Claim::Decline(error.to_string()),
            }
        })
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            match phab_app_search::run(work.database, work.config, work.data(), crate::time()).await
            {
                Ok(Outcome::Done) => Ok(()),
                Ok(Outcome::Yield(duration)) => Err(Failure::Yield(duration)),
                Ok(Outcome::Failed(message)) => Err(Failure::Permanent(message)),
                Err(Error::Unsupported(reason)) => Err(Failure::Decline(reason)),
                Err(Error::Db(error)) => Err(Failure::Temporary(error.to_string())),
            }
        })
    }
}
