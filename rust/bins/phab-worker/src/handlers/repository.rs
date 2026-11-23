//! The commit import steps the discovery engine and the ref engine queue
//! for each Git commit ([`phab_app_diffusion::import`]):
//! `PhabricatorRepositoryGitCommitMessageParserWorker`, which queues
//! `PhabricatorRepositoryGitCommitChangeParserWorker`, which queues
//! `PhabricatorRepositoryCommitPublishWorker`.
//!
//! Each step reads what it needs first (the task's commit and repository,
//! Git's output); a task it can't do as PHP does is declined before the
//! lease when possible: Mercurial and Subversion, repositories served by a
//! cluster or whose working copy this host doesn't have, output which is
//! not UTF-8, commits with an encoding header, and what the publishing of
//! a commit needs which is not ported ([`publish`]).

use phab_app_diffusion::import::commit::{FollowUp, ParserTask};
use phab_app_diffusion::import::{
    CHANGE_PARSER, ImportError, MESSAGE_PARSER, PUBLISHER, change, message, publish,
};
use phab_app_files::mime::MimeCommand;
use phab_db::Database;
use serde_json::Value;

use crate::handler::{
    Claim, ClaimFuture, Failure, Handler, QueuedTask, Unleased, Work, WorkFuture,
};
use crate::task::ActiveTask;

/// `PhabricatorRepositoryGitCommitMessageParserWorker`.
#[derive(Debug, Clone, Copy, Default)]
pub struct MessageParserWorker;

/// `PhabricatorRepositoryGitCommitChangeParserWorker`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChangeParserWorker;

/// `PhabricatorRepositoryCommitPublishWorker`, which detects the MIME type
/// of patches with its command (without one, publishing is left to PHP).
#[derive(Debug, Clone, Default)]
pub struct PublishWorker {
    mime: Option<MimeCommand>,
}

impl PublishWorker {
    /// The worker, detecting MIME types with `PHAB_MIME_COMMAND`.
    pub fn from_env() -> Self {
        Self::with_mime(MimeCommand::from_env())
    }

    /// The worker, detecting MIME types with `mime`.
    pub fn with_mime(mime: Option<MimeCommand>) -> Self {
        Self { mime }
    }
}

/// How an error of a step ends the task: before anything is written,
/// what PHP might do differently is left to it.
fn before_writes(error: ImportError) -> Failure {
    match error {
        ImportError::Permanent(message) => Failure::Permanent(message),
        other => Failure::Decline(other.to_string()),
    }
}

/// Once the step has written: PHP's exception, a temporary failure.
fn after_writes(error: ImportError) -> Failure {
    match error {
        ImportError::Permanent(message) => Failure::Permanent(message),
        other => Failure::Temporary(other.to_string()),
    }
}

/// What the lease query decides from a failure found before the lease:
/// a missing commit is PHP's permanent failure, which Rust writes too.
fn claim(result: Result<(), Failure>) -> Claim {
    match result {
        Ok(()) | Err(Failure::Permanent(_)) => Claim::Take,
        Err(failure) => Claim::Decline(failure.to_string()),
    }
}

async fn load(database: &Database, data: &Value) -> Result<ParserTask, Failure> {
    ParserTask::load(database, data)
        .await
        .map_err(before_writes)
}

fn queue(work: &Work<'_>, followup: Option<FollowUp>) {
    if let Some(followup) = followup {
        work.queue_task(QueuedTask {
            class: followup.class.to_owned(),
            data: followup.data,
            priority: Some(followup.priority),
            object_phid: followup.object_phid,
            container_phid: followup.container_phid,
        });
    }
}

impl Handler for MessageParserWorker {
    fn class(&self) -> &'static str {
        MESSAGE_PARSER
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(async move {
            claim(
                async {
                    let parser = load(task.database, task.data).await?;
                    message::prepare(task.database, &parser)
                        .await
                        .map_err(before_writes)?;
                    Ok(())
                }
                .await,
            )
        })
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            let mut parser = load(work.database, work.data()).await?;
            let commit_ref = message::prepare(work.database, &parser)
                .await
                .map_err(before_writes)?;
            let parsed = message::parse(work.database, &mut parser, commit_ref)
                .await
                .map_err(after_writes)?;
            queue(work, parsed.followup);
            Ok(())
        })
    }
}

impl Handler for ChangeParserWorker {
    fn class(&self) -> &'static str {
        CHANGE_PARSER
    }

    /// 24 hours: some commits change millions of paths.
    fn required_lease_time(&self, _task: &ActiveTask) -> Result<Option<i64>, Failure> {
        Ok(Some(24 * 60 * 60))
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(async move {
            claim(
                async {
                    let parser = load(task.database, task.data).await?;
                    change::prepare(task.database, &parser)
                        .await
                        .map_err(before_writes)?;
                    Ok(())
                }
                .await,
            )
        })
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            let mut parser = load(work.database, work.data()).await?;
            let prepared = change::prepare(work.database, &parser)
                .await
                .map_err(before_writes)?;
            let followup = change::parse(work.database, &mut parser, prepared)
                .await
                .map_err(after_writes)?;
            queue(work, followup);
            Ok(())
        })
    }
}

impl Handler for PublishWorker {
    fn class(&self) -> &'static str {
        PUBLISHER
    }

    /// 4 hours: Herald rules may take long.
    fn required_lease_time(&self, _task: &ActiveTask) -> Result<Option<i64>, Failure> {
        Ok(Some(4 * 60 * 60))
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(async move {
            claim(
                async {
                    let parser = load(task.database, task.data).await?;
                    let context = crate::context(task.database, task.config);
                    publish::prepare(&context, &parser, self.mime.as_ref())
                        .await
                        .map_err(before_writes)?;
                    Ok(())
                }
                .await,
            )
        })
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            let mut parser = load(work.database, work.data()).await?;
            let context = crate::context(work.database, work.config);
            let prepared = publish::prepare(&context, &parser, self.mime.as_ref())
                .await
                .map_err(before_writes)?;
            // The last step: only the closing of a revision is queued.
            let followup = publish::parse(work.database, &mut parser, prepared)
                .await
                .map_err(after_writes)?;
            queue(work, followup);
            Ok(())
        })
    }
}
