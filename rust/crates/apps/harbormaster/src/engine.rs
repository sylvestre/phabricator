//! The build engine (`HarbormasterBuildEngine`) and the two task classes
//! which drive it: `HarbormasterBuildWorker`, queued for a build when
//! someone sends it a message or a plan starts it, or for a buildable
//! (`buildablePHID`) when Differential is done applying build plans; and
//! `HarbormasterTargetWorker`, queued for each build target the engine
//! creates.
//!
//! Like the other ported writes, a task is planned first: everything is
//! read, then the engine runs over that state in memory
//! (`continueBuild()`, `updateBuildable()`, `publishBuildable()`),
//! collecting the rows PHP would write in order. Anything not ported is
//! [`EngineError::Unsupported`] at that point, before anything is written,
//! so the worker can leave the task to PHP before leasing it. Then one
//! MySQL transaction writes the rows.
//!
//! Ported, for builds of the automatic or manual buildables of diffs:
//!
//! - the build commands (`restart`, `abort`, `pause`, `resume`), as the
//!   `message/*` transactions of `HarbormasterBuildTransactionEditor` with
//!   their `PhabricatorApplicationTransactionPublishWorker` task, and the
//!   start of pending builds;
//! - the target messages (`pass`, `fail`, `work`), the step states, the
//!   build status transitions (passed, failed, deadlocked), new targets for
//!   the steps whose dependencies completed, with their variables and
//!   `HarbormasterTargetWorker` tasks, and artifact releases;
//! - the buildable messages (`build`, `container`) and the buildable status
//!   (`buildable` plan behavior), and Differential's side of publishing a
//!   buildable (`DifferentialBuildableEngine::publishBuildable()`): the
//!   revision's `harbormaster:buildable` transaction, through Differential's
//!   editor ([`phab_app_differential::edit::publish_buildable()`]), planned
//!   with the rest before anything is written, and written in its own
//!   transaction where PHP's engine publishes.
//!
//! [`target`] runs the targets of "Make HTTP Request" steps (the only step
//! of Mozilla's plans) and of the Arcanist lint and unit autotargets.
//!
//! Left to PHP: buildables of commits and other objects; targets for diffs
//! of revisions with a repository (their variables need the repository's
//! clone URIs); a buildable status change whose revision edit Differential's
//! editor leaves to PHP; restarts of plans
//! restartable only if failed (PHP fails with an `Error` there); builds
//! whose plan is gone; step implementations which are not installed; rows
//! whose JSON PHP would encode differently on save; and lock logs
//! (`daemon.lock-log` retention).

mod http;
mod lock;
mod log;
mod rows;
mod sim;
pub mod target;
mod write;

use phab_conduit::Context;
use phab_core::Config;
use phab_db::DbError;
use serde_json::Value;
use thiserror::Error;

pub use http::{HttpClient, HttpFuture, HttpRequest, HttpResponse};

/// `HarbormasterBuildWorker`.
pub const BUILD_WORKER: &str = "HarbormasterBuildWorker";

/// `HarbormasterTargetWorker`.
pub const TARGET_WORKER: &str = "HarbormasterTargetWorker";

/// `PhabricatorHarbormasterApplication`'s PHID: the acting PHID of the
/// omnipotent viewer the workers use.
const HARBORMASTER_APP: &str = "PHID-APPS-PhabricatorHarbormasterApplication";

/// How a task ends without succeeding.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The task needs code which is not ported; nothing was written.
    #[error("left to PHP: {0}")]
    Unsupported(String),
    /// `PhabricatorWorkerPermanentFailureException`.
    #[error("{0}")]
    Permanent(String),
    /// Another exception: the task is retried.
    #[error("{0}")]
    Failed(String),
    /// A query failed.
    #[error(transparent)]
    Database(#[from] DbError),
}

impl From<phab_db::sqlx::Error> for EngineError {
    fn from(error: phab_db::sqlx::Error) -> Self {
        Self::Database(DbError::from(error))
    }
}

/// Shorthand for [`EngineError::Unsupported`].
fn unsupported(what: impl Into<String>) -> EngineError {
    EngineError::Unsupported(what.into())
}

/// Whether the engine only plans (before the lease: no lock, no write) or
/// runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Read and plan, to decide whether to lease the task.
    Inspect,
    /// Take the locks, plan again and write.
    Run,
}

/// A task queued with `PhabricatorWorker::scheduleTask()` while the engine
/// ran, in the order PHP queues them.
#[derive(Debug, Clone, PartialEq)]
pub struct Scheduled {
    /// The task class.
    pub class: String,
    /// The task data, as PHP's `json_encode()` writes it.
    pub data: String,
    /// The priority.
    pub priority: i64,
    /// `objectPHID`.
    pub object_phid: Option<String>,
}

/// Checks the configuration the engine depends on: PHP logs its global
/// locks when `daemon.lock-log` has a retention policy.
fn check_config(config: &Config) -> Result<(), EngineError> {
    if let Some(Value::Object(policies)) = config.value("phd.garbage-collection")
        && policies
            .get("daemon.lock-log")
            .is_some_and(phab_core::php::is_truthy)
    {
        return Err(unsupported("global lock logs (daemon.lock-log)"));
    }
    Ok(())
}

/// `HarbormasterBuildWorker::doWork()` for the task data `data`, at `now`.
pub async fn build_worker(
    context: &Context,
    data: &Value,
    now: i64,
    mode: Mode,
) -> Result<(), EngineError> {
    check_config(&context.config)?;
    let Value::Object(map) = data else {
        return Err(unsupported("build tasks whose data is not a dictionary"));
    };
    let build_id = map.get("buildID");
    if build_id.is_some_and(phab_core::php::is_truthy) {
        let id = rows::id_param(build_id)?;
        sim::continue_build_task(context, id, now, mode).await
    } else {
        let phid = match map.get("buildablePHID") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(phid)) => phid.clone(),
            Some(_) => return Err(unsupported("buildable PHIDs which are not strings")),
        };
        sim::update_buildable_task(context, &phid, now, mode).await
    }
}
