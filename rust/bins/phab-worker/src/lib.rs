//! Phabricator's task queue (`phabricator_worker`), run from Rust.
//!
//! PHP's taskmasters (`PhabricatorTaskmasterDaemon`, started by `phd`) and
//! this worker share the queue. The worker leases only the task classes it
//! has a [`Handler`] for, so every other task stays in the queue for PHP,
//! and it writes the rows PHP's `PhabricatorWorkerActiveTask` and
//! `PhabricatorWorkerArchiveTask` would write.

pub mod daemon;
pub mod execute;
pub mod handler;
pub mod handlers;
pub mod lease;
pub mod task;

pub use daemon::{Ran, Worker};
pub use execute::{ExecuteError, Executed, execute};
pub use handler::{
    Claim, ClaimFuture, Failure, Handler, QueuedTask, Registry, Unleased, Work, WorkFuture,
};
pub use lease::{Declined, Inspection, LeaseOwner, LeaseQuery};
pub use task::{ActiveTask, ArchiveTask, PriorLease, TaskResult};

/// `PhabricatorWorker::YIELD_OWNER`: the lease owner of a task which has
/// yielded.
pub const YIELD_OWNER: &str = "(yield)";

/// `PhabricatorWorker::PRIORITY_DEFAULT`.
pub const PRIORITY_DEFAULT: i64 = 2000;
/// PHP's `time()`.
pub(crate) fn time() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(elapsed).unwrap_or(i64::MAX)
}
