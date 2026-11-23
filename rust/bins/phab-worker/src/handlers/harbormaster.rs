//! Harbormaster's workers ([`phab_app_harbormaster::engine`]):
//! `HarbormasterBuildWorker`, which continues a build after a message or
//! updates a buildable, and `HarbormasterTargetWorker`, which runs a build
//! target's step. Both plan the whole task before the lease, and decline
//! what isn't ported then.

use std::sync::Arc;

use phab_app_harbormaster::engine::{self, EngineError, HttpClient, Mode};
use phab_core::Config;
use phab_db::Database;
use serde_json::Value;

use crate::handler::{Claim, ClaimFuture, Failure, Handler, Unleased, Work, WorkFuture};
use crate::task::ActiveTask;

/// `HarbormasterBuildWorker`.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildWorker;

/// `HarbormasterTargetWorker`, which sends the requests of HTTP steps with
/// its client; without one, those targets are left to PHP.
#[derive(Clone, Default)]
pub struct TargetWorker {
    client: Option<Arc<dyn HttpClient>>,
}

impl TargetWorker {
    /// The worker, sending requests with `client`.
    pub fn with_client(client: Arc<dyn HttpClient>) -> Self {
        Self {
            client: Some(client),
        }
    }
}

/// How an engine error ends the task.
fn failure(error: EngineError) -> Failure {
    match error {
        EngineError::Unsupported(reason) => Failure::Decline(reason),
        EngineError::Permanent(message) => Failure::Permanent(message),
        EngineError::Failed(message) => Failure::Temporary(message),
        EngineError::Database(error) => Failure::Temporary(error.to_string()),
    }
}

/// Whether to lease a task, from its plan: a permanent failure is PHP's
/// too, which Rust writes as well; anything else is left to PHP.
fn claim(result: Result<(), EngineError>) -> Claim {
    match result {
        Ok(()) | Err(EngineError::Permanent(_)) => Claim::Take,
        Err(error) => Claim::Decline(error.to_string()),
    }
}

async fn build(
    data: &Value,
    database: &Database,
    config: &Config,
    mode: Mode,
) -> Result<(), EngineError> {
    let context = crate::context(database, config);
    engine::build_worker(&context, data, crate::time(), mode).await
}

impl Handler for BuildWorker {
    fn class(&self) -> &'static str {
        engine::BUILD_WORKER
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(
            async move { claim(build(task.data, task.database, task.config, Mode::Inspect).await) },
        )
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            build(work.data(), work.database, work.config, Mode::Run)
                .await
                .map_err(failure)
        })
    }
}

impl TargetWorker {
    async fn run(
        &self,
        data: &Value,
        database: &Database,
        config: &Config,
        mode: Mode,
    ) -> Result<(), EngineError> {
        engine::target::target_worker(
            &crate::context(database, config),
            data,
            crate::time(),
            self.client.as_deref(),
            mode,
        )
        .await
    }
}

impl Handler for TargetWorker {
    fn class(&self) -> &'static str {
        engine::TARGET_WORKER
    }

    /// `phutil_units('24 hours in seconds')`.
    fn required_lease_time(&self, _task: &ActiveTask) -> Result<Option<i64>, Failure> {
        Ok(Some(24 * 60 * 60))
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(async move {
            claim(
                self.run(task.data, task.database, task.config, Mode::Inspect)
                    .await,
            )
        })
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            self.run(work.data(), work.database, work.config, Mode::Run)
                .await
                .map_err(failure)
        })
    }
}
