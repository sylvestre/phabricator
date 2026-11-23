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
pub mod repository_command;
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
/// The context of the handlers' Conduit calls, editors and Herald
/// transcripts: the host name is `php_uname('n')`.
pub(crate) fn context(
    database: &phab_db::Database,
    config: &phab_core::Config,
) -> phab_conduit::Context {
    phab_conduit::Context {
        database: database.clone(),
        config: config.clone(),
        hostname: rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned(),
    }
}

/// `PhabricatorEnv`'s stack (defaults, `local.json` in `config_dir`, the
/// database), then the connection.
pub async fn connect(
    config_dir: &std::path::Path,
) -> Result<(phab_core::Config, phab_db::Database), String> {
    let mut config = phab_core::Config::default();
    config
        .push_local_file(&config_dir.join("local.json"))
        .map_err(|error| error.to_string())?;
    let database = phab_db::Database::connect(&config)
        .await
        .map_err(|error| error.to_string())?;
    let entries = database
        .load_config_entries()
        .await
        .map_err(|error| error.to_string())?;
    config.push_layer("Database", entries);
    Ok((config, database))
}

/// `PHAB_CONFIG_DIR`, or the directory of `local.json` in the PHP image.
pub fn config_dir() -> std::path::PathBuf {
    std::env::var_os("PHAB_CONFIG_DIR").map_or_else(
        || std::path::PathBuf::from("/app/phabricator/conf/local"),
        std::path::PathBuf::from,
    )
}

/// PHP's `time()`.
pub(crate) fn time() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(elapsed).unwrap_or(i64::MAX)
}
