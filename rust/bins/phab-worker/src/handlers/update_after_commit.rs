//! `DiffusionUpdateObjectAfterCommitWorker`, which the publishing of a
//! commit queues for the revision its message names
//! (`crates/apps/diffusion/src/import/publish/close.rs`): it closes the
//! revision with the commit (`DifferentialDiffExtractionEngine::
//! updateRevisionWithCommit()`).
//!
//! Ported: the task's checks, which fail it for good (no commit or object
//! PHID, a commit or an object which doesn't load), as PHP does.
//!
//! Left to PHP: closing a revision. It needs the commit's diff
//! (`newDiffFromCommit()`: `diffusion.rawdiffquery`'s temporary file, the
//! parsed changesets and their binary files, `local:commits`), whether the
//! diff changed since the revision's (`isDiffChangedBeforeCommit()`), and
//! the build plans' landing warnings, before the editor's
//! `differential:update`, `core:edge` and `differential.revision.close`
//! transactions; and objects of other types, which PHP loads but leaves as
//! they are.

use phab_core::php;
use phab_db::{Database, DbError};
use serde_json::Value;

use crate::handler::{Claim, ClaimFuture, Failure, Handler, Unleased, Work, WorkFuture};

/// `DiffusionUpdateObjectAfterCommitWorker`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateObjectAfterCommitWorker;

/// `idx($data, $key)` when PHP finds it truthy.
fn truthy<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    data.get(key).filter(|value| php::is_truthy(value))
}

/// Whether `DiffusionCommitQuery` loads the commit: it exists, with its
/// repository.
async fn commit_exists(database: &Database, phid: &str) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT 1 FROM {} c JOIN {} r ON r.id = c.repositoryID WHERE c.phid = ?",
        database.table("repository", "repository_commit"),
        database.table("repository", "repository")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// Whether the revision `phid` exists.
async fn revision_exists(database: &Database, phid: &str) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT 1 FROM {} WHERE phid = ?",
        database.table("differential", "differential_revision")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// `doWork()` up to the update of the object: the permanent failures PHP
/// raises, or the update, which is left to PHP.
async fn run(database: &Database, data: &Value) -> Result<(), Failure> {
    let temporary = |error: DbError| Failure::Temporary(error.to_string());
    if !data.is_object() {
        return Err(Failure::Decline(
            "task data which is not a dictionary".to_owned(),
        ));
    }
    let Some(commit) = truthy(data, "commitPHID") else {
        return Err(Failure::Permanent(
            "No \"commitPHID\" in task data.".to_owned(),
        ));
    };
    let Value::String(commit) = commit else {
        return Err(Failure::Decline(
            "commit PHIDs which are not strings".to_owned(),
        ));
    };
    if !commit_exists(database, commit).await.map_err(temporary)? {
        return Err(Failure::Permanent(format!(
            "Unable to load commit \"{commit}\"."
        )));
    }
    let Some(object) = truthy(data, "objectPHID") else {
        return Err(Failure::Permanent(
            "No \"objectPHID\" in task data.".to_owned(),
        ));
    };
    let Value::String(object) = object else {
        return Err(Failure::Decline(
            "object PHIDs which are not strings".to_owned(),
        ));
    };
    if !object.starts_with("PHID-DREV-") {
        return Err(Failure::Decline("objects other than revisions".to_owned()));
    }
    if !revision_exists(database, object).await.map_err(temporary)? {
        return Err(Failure::Permanent(format!(
            "Unable to load object \"{object}\"."
        )));
    }
    Err(Failure::Decline(
        "closing revisions with commits (newDiffFromCommit())".to_owned(),
    ))
}

impl Handler for UpdateObjectAfterCommitWorker {
    fn class(&self) -> &'static str {
        phab_app_diffusion::import::publish::close::UPDATE_WORKER
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(async move {
            match run(task.database, task.data).await {
                Ok(()) | Err(Failure::Permanent(_)) => Claim::Take,
                Err(failure) => Claim::Decline(failure.to_string()),
            }
        })
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move { run(work.database, work.data()).await })
    }
}
