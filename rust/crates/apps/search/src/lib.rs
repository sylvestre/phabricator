//! Search indexing (`PhabricatorSearchWorker`): the Ferret fulltext index
//! (`PhabricatorFerretFulltextEngineExtension`) of Differential revisions,
//! users and projects, their `search_indexversion` rows, the datasource
//! tokens of projects and their materialized members.
//!
//! [`inspect`] reads what a task needs and builds every row without
//! writing, so what Rust can't build as PHP does is [`Error::Unsupported`]
//! before the task is leased; [`run`] is `doWork()`. `cluster.search`
//! only knows the `mysql` engine, which is Ferret's and stores nothing
//! itself: the `search_document*` tables are gone.

pub mod ferret;
pub mod index;
pub mod object;
mod php_sort;
pub mod porter;
pub mod text;

use phab_core::Config;
use phab_db::{Database, DbError};
use serde_json::Value;

/// Why a task could not be run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The task needs code which is not ported.
    #[error("not ported: {0}")]
    Unsupported(String),
}

impl Error {
    /// An [`Error::Unsupported`].
    pub(crate) fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported(reason.into())
    }
}

impl From<text::Unsupported> for Error {
    fn from(error: text::Unsupported) -> Self {
        Self::Unsupported(error.0.to_owned())
    }
}

impl From<phab_db::sqlx::Error> for Error {
    fn from(error: phab_db::sqlx::Error) -> Self {
        Self::Db(error.into())
    }
}

/// How `doWork()` ended, when Rust ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It returned.
    Done,
    /// `PhabricatorWorkerYieldException`: another process holds the lock.
    Yield(i64),
    /// `PhabricatorWorkerPermanentFailureException`, thrown in strict mode.
    Failed(String),
}

/// Reads and builds what [`run`] would write for this task, without the
/// lock.
pub async fn inspect(database: &Database, config: &Config, data: &Value) -> Result<(), Error> {
    let task = index::parse_task(data)?;
    let Some(object) = index::load_object(database, config, &task).await? else {
        return Ok(());
    };
    let indexing = index::plan(database, config, &task, &object).await?;
    if indexing.fulltext || indexing.members {
        index::prepare(database, config, &indexing, object).await?;
    }
    Ok(())
}

/// `PhabricatorSearchWorker::doWork()`.
pub async fn run(
    database: &Database,
    config: &Config,
    data: &Value,
    now: i64,
) -> Result<Outcome, Error> {
    let task = index::parse_task(data)?;
    let Some(object) = index::load_object(database, config, &task).await? else {
        return Ok(missing(&task));
    };
    let indexing = index::plan(database, config, &task, &object).await?;
    if !indexing.fulltext && !indexing.members {
        return Ok(Outcome::Done);
    }
    let Some(lock) = index::Lock::acquire(database, &task.phid).await? else {
        return Ok(Outcome::Yield(index::LOCKED_YIELD));
    };
    let result = run_locked(database, config, &task, &indexing, now).await;
    // PHP ignores a failed unlock.
    if let Err(error) = lock.release().await {
        tracing::warn!(%error, phid = task.phid, "releasing the index lock failed");
    }
    result
}

/// The object doesn't load: an error only in strict mode.
fn missing(task: &index::Task) -> Outcome {
    if task.strict {
        Outcome::Failed(format!(
            "Unable to load object \"{}\" to rebuild indexes.",
            task.phid
        ))
    } else {
        Outcome::Done
    }
}

/// Reloads the object under the lock and indexes it. Failures end the
/// task as PHP's `catch` does: silently, unless it is strict.
async fn run_locked(
    database: &Database,
    config: &Config,
    task: &index::Task,
    indexing: &index::Indexing,
    now: i64,
) -> Result<Outcome, Error> {
    let written = async {
        let Some(object) = index::load_object(database, config, task).await? else {
            return Ok(Some(missing(task)));
        };
        let prepared = index::prepare(database, config, indexing, object).await?;
        index::write(database, indexing, &prepared, now).await?;
        Ok::<_, Error>(None)
    }
    .await;
    match written {
        Ok(Some(outcome)) => Ok(outcome),
        Ok(None) => Ok(Outcome::Done),
        Err(error @ Error::Unsupported(_)) => Err(error),
        Err(Error::Db(error)) => {
            tracing::warn!(%error, phid = task.phid, "indexing failed");
            if task.strict {
                Ok(Outcome::Failed(format!(
                    "Failed to update search index for document \"{}\": {error}",
                    task.phid
                )))
            } else {
                Ok(Outcome::Done)
            }
        }
    }
}
