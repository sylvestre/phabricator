//! The repository import pipeline for Git: what `bin/repository discover`,
//! `refs` and `update` do to an observed repository
//! (`PhabricatorRepositoryDiscoveryEngine`, `PhabricatorRepositoryRefEngine`,
//! `PhabricatorRepositoryPullEngine`), and the tasks they queue for each
//! commit (`PhabricatorRepositoryGitCommitMessageParserWorker`,
//! `PhabricatorRepositoryGitCommitChangeParserWorker`,
//! `PhabricatorRepositoryCommitPublishWorker`).
//!
//! Every step writes the rows PHP's writes, in the same order, or leaves
//! the work to PHP ([`ImportError::Unsupported`]) before it writes
//! anything when it needs code which is not ported: Mercurial and
//! Subversion, repositories served by a cluster, hosted repositories (whose
//! hooks PHP installs), mirrors, branch filters with regular expressions,
//! event listeners, lock logs, and the publishing of a commit (Herald,
//! audits, feed, mail and the closing of revisions).

pub mod change;
pub mod commit;
pub mod discovery;
pub mod graph;
pub mod identity;
pub mod lock;
pub mod message;
pub mod publish;
pub mod pull;
pub mod ref_engine;
pub mod repository;
pub mod text;
pub mod update;

use phab_conduit::MethodError;
use phab_core::{Config, php};
use phab_db::worker::{Task, schedule_contained_task};
use phab_db::{Database, DbError};
use serde_json::{Value, json};
use thiserror::Error;

use crate::import::repository::ImportRepository;

/// `PhabricatorRepositoryCommit::IMPORTED_MESSAGE`.
pub const IMPORTED_MESSAGE: i64 = 1;
/// `PhabricatorRepositoryCommit::IMPORTED_CHANGE`.
pub const IMPORTED_CHANGE: i64 = 2;
/// `PhabricatorRepositoryCommit::IMPORTED_PUBLISH`.
pub const IMPORTED_PUBLISH: i64 = 8;
/// `PhabricatorRepositoryCommit::IMPORTED_ALL`.
pub const IMPORTED_ALL: i64 = 11;
/// `PhabricatorRepositoryCommit::IMPORTED_PERMANENT`.
pub const IMPORTED_PERMANENT: i64 = 1024;
/// `PhabricatorRepositoryCommit::IMPORTED_UNREACHABLE`.
pub const IMPORTED_UNREACHABLE: i64 = 2048;

/// `PhabricatorWorker::PRIORITY_DEFAULT`.
pub const PRIORITY_DEFAULT: i64 = 2000;
/// `PhabricatorWorker::PRIORITY_COMMIT`.
pub const PRIORITY_COMMIT: i64 = 2500;
/// `PhabricatorWorker::PRIORITY_INDEX`.
pub const PRIORITY_INDEX: i64 = 3500;
/// `PhabricatorWorker::PRIORITY_IMPORT`.
pub const PRIORITY_IMPORT: i64 = 4000;

/// `PhabricatorRepository::IMPORT_THRESHOLD`.
pub const IMPORT_THRESHOLD: usize = 7;
/// `PhabricatorRepository::LOWPRI_THRESHOLD`.
pub const LOWPRI_THRESHOLD: usize = 64;

/// The task class of the first step of a Git commit's import.
pub const MESSAGE_PARSER: &str = "PhabricatorRepositoryGitCommitMessageParserWorker";
/// The task class of the second step.
pub const CHANGE_PARSER: &str = "PhabricatorRepositoryGitCommitChangeParserWorker";
/// The task class of the last step.
pub const PUBLISHER: &str = "PhabricatorRepositoryCommitPublishWorker";
/// `PhabricatorSearchWorker`.
pub const SEARCH_WORKER: &str = "PhabricatorSearchWorker";

/// Why a step stopped.
#[derive(Debug, Error)]
pub enum ImportError {
    /// The step needs code which is not ported; nothing was written yet,
    /// unless the step says otherwise.
    #[error("left to PHP: {0}")]
    Unsupported(String),
    /// What PHP raises as an exception: a temporary failure of a task, an
    /// error of a command.
    #[error("{0}")]
    Failed(String),
    /// `PhabricatorWorkerPermanentFailureException`.
    #[error("{0}")]
    Permanent(String),
    /// `DiffusionDaemonLockException`: another process holds the lock.
    #[error("{0}")]
    Locked(String),
    /// A query failed.
    #[error(transparent)]
    Database(#[from] DbError),
}

impl ImportError {
    /// [`ImportError::Unsupported`].
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported(reason.into())
    }
}

impl From<sqlx::Error> for ImportError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error.into())
    }
}

impl From<MethodError> for ImportError {
    fn from(error: MethodError) -> Self {
        match error {
            MethodError::Unsupported(reason) => Self::Unsupported(reason.to_owned()),
            MethodError::Conduit { code, info } => Self::Failed(info.unwrap_or(code)),
            MethodError::Internal(message) => Self::Failed(message),
        }
    }
}

/// PHP's `time()`.
pub fn now() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(elapsed).unwrap_or(i64::MAX)
}

/// The configuration the steps can't follow: event listeners, which PHP
/// calls when a commit is discovered, and lock logs, which it writes when
/// it takes a repository lock.
pub fn check_config(config: &Config) -> Result<(), ImportError> {
    if config.value("events.listeners").is_some_and(php::is_truthy) {
        return Err(ImportError::unsupported("event listeners"));
    }
    let lock_log = match config.value("phd.garbage-collection") {
        Some(Value::Object(policies)) => policies.get("daemon.lock-log").cloned(),
        _ => None,
    };
    if lock_log.as_ref().is_some_and(php::is_truthy) {
        return Err(ImportError::unsupported("lock logs"));
    }
    Ok(())
}

/// `PhabricatorRepositoryEngine::getImportTaskPriority()`.
pub fn import_task_priority(repository: &ImportRepository, count: usize) -> i64 {
    if repository.is_importing() || count >= LOWPRI_THRESHOLD {
        PRIORITY_IMPORT
    } else {
        PRIORITY_COMMIT
    }
}

/// `PhabricatorRepositoryEngine::queueCommitImportTask()`:
/// `PhabricatorWorker::scheduleTask()` of the first step, at once.
pub async fn queue_commit_import_task(
    database: &Database,
    repository: &ImportRepository,
    commit_phid: &str,
    priority: i64,
    via: &str,
) -> Result<(), ImportError> {
    let data = php::json_encode(&json!({"commitPHID": commit_phid, "via": via}));
    let mut connection = database.pool().acquire().await?;
    schedule_contained_task(
        &mut connection,
        database,
        Task {
            class: MESSAGE_PARSER,
            data: &data,
            priority,
            object_phid: Some(commit_phid),
        },
        Some(repository.phid.as_str()),
        now(),
    )
    .await?;
    Ok(())
}
