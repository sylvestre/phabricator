//! `PhabricatorRepositoryCommitPublishWorker`: the last step.
//!
//! A commit the publisher holds (`getCommitHoldReasons()`: the repository
//! is importing or doesn't publish, or the commit is on no permanent ref)
//! only gets its hold reasons and the `IMPORTED_PUBLISH` flag.
//!
//! A commit which publishes (`publishCommit()`) gets, in PHP's order:
//! - its hold reasons cleared, and the revision its message names
//!   ("Differential Revision: ...", with Mozilla's
//!   `diffusion.legacy-repos-mapping`) recorded in its data
//!   ([`close`]); the revision itself is closed by the
//!   `DiffusionUpdateObjectAfterCommitWorker` task this queues, which PHP
//!   runs;
//! - its patch, stored as a temporary file ([`patch`]);
//! - the `audit:commit` transaction of `PhabricatorAuditEditor`, Herald's
//!   transcript and the transaction publish task with the editor's state
//!   ([`audit`]), whose feed story and mail PHP sends;
//! - Mozilla's merge conflict rechecks of the open revisions touching the
//!   files it changed ([`rechecks`]).
//!
//! Left to PHP, before anything is written: commits with audits or which
//! the message asks to audit, enabled Herald rules on commits (whose fields
//! are not ported), Harbormaster buildables of the commit, messages which
//! mention users or reference objects other than the revision they close,
//! reverts or dependencies, revisions found by commit hash, commits not
//! mostly imported (`bin/repository reparse`), commits with projects, and
//! patches which are too large, too slow or which Git fails to make.

pub mod audit;
pub mod close;
pub mod patch;
pub mod rechecks;

use phab_app_files::mime::MimeCommand;
use phab_conduit::{ApiRequest, Context};
use phab_core::php;
use phab_db::Database;
use phab_policy::Viewer;
use serde_json::{Map, Value, json};

use crate::import::commit::{CommitData, FollowUp, ParserTask};
use crate::import::repository::HOLD_NOT_REACHABLE_FROM_PERMANENT_REF;
use crate::import::{IMPORTED_PUBLISH, ImportError, now};

/// `getCommitHoldReasons()`.
pub fn hold_reasons(task: &ParserTask) -> Vec<&'static str> {
    let mut reasons = task.repository.hold_reasons();
    if !task.commit.is_permanent() {
        reasons.push(HOLD_NOT_REACHABLE_FROM_PERMANENT_REF);
    }
    reasons
}

/// What the step will do.
#[derive(Debug)]
pub enum Prepared {
    /// The step was done before.
    Skip,
    /// The commit is held: its data and the reasons.
    Hold(CommitData, Vec<&'static str>),
    /// The commit publishes.
    Publish(Box<Plan>),
}

/// Everything the publishing of a commit writes, planned before any of it.
#[derive(Debug)]
pub struct Plan {
    /// The commit's data, with its new details.
    data: CommitData,
    /// Whether the data changed and is saved.
    data_changed: bool,
    /// The task closing the revision, queued once the step is done.
    close: Option<FollowUp>,
    /// The patch, stored as a file.
    patch: patch::Patch,
    /// The editor's writes.
    audit: audit::Edit,
    /// The merge conflict rechecks.
    rechecks: Vec<rechecks::Check>,
    /// When the step runs.
    now: i64,
}

/// A Conduit call made by the worker (`ConduitCall` with the omnipotent
/// user).
pub(crate) fn request(context: &Context, params: Map<String, Value>, now: i64) -> ApiRequest<'_> {
    ApiRequest {
        context,
        user: None,
        viewer: Viewer::omnipotent(),
        params,
        strictly_typed: true,
        now,
        writes: phab_auth::Writes::Allowed,
    }
}

/// Checks the step can be done here, and plans it when the commit
/// publishes; `mime` detects the type of the patch's file. The commit must
/// have its data row (which `DiffusionCommitQuery::needCommitData()`
/// attaches).
pub async fn prepare(
    context: &Context,
    task: &ParserTask,
    mime: Option<&MimeCommand>,
) -> Result<Prepared, ImportError> {
    if task.should_skip_step(IMPORTED_PUBLISH) {
        return Ok(Prepared::Skip);
    }
    let database = &context.database;
    let Some(mut data) = CommitData::load_existing(database, task.commit.id).await? else {
        return Err(ImportError::unsupported("a commit without its data"));
    };
    let reasons = hold_reasons(task);
    if !reasons.is_empty() {
        return Ok(Prepared::Hold(data, reasons));
    }
    let now = now();
    // `!==` of the stored reasons and an empty list.
    let mut data_changed = data.detail("holdReasons").map(php::round_trip) != Some(json!([]));
    if data_changed {
        data.set_detail("holdReasons", json!([]));
    }
    let closing = close::plan(context, task, &data, now).await?;
    let close = match closing {
        Some(closing) => {
            closing.apply(&mut data);
            data_changed = true;
            Some(closing.followup(task))
        }
        None => None,
    };
    let mut audit = audit::plan(context, task, &data, now).await?;
    let patch = patch::load(context, task, mime, now).await?;
    audit.set_raw_patch(&patch.text);
    let rechecks = rechecks::plan(context, task).await?;
    Ok(Prepared::Publish(Box::new(Plan {
        data,
        data_changed,
        close,
        patch,
        audit,
        rechecks,
        now,
    })))
}

/// `parseCommit()`: the hold reasons of a held commit are saved when they
/// changed (`!==`); a commit which publishes gets the writes of its plan.
/// Then the flag. Returns the task closing the revision, which
/// `queueTask()` queues once the step is done.
pub async fn parse(
    database: &Database,
    task: &mut ParserTask,
    prepared: Prepared,
) -> Result<Option<FollowUp>, ImportError> {
    let followup = match prepared {
        Prepared::Skip => return Ok(None),
        Prepared::Hold(mut data, reasons) => {
            let reasons = json!(reasons);
            let stored = data.detail("holdReasons").map(php::round_trip);
            if stored.as_ref() != Some(&reasons) {
                data.set_detail("holdReasons", reasons);
                data.save(database).await?;
            }
            None
        }
        Prepared::Publish(plan) => publish(database, task, *plan).await?,
    };
    task.commit
        .write_import_status_flag(database, IMPORTED_PUBLISH)
        .await?;
    Ok(followup)
}

/// `publishCommit()` as planned.
async fn publish(
    database: &Database,
    task: &mut ParserTask,
    mut plan: Plan,
) -> Result<Option<FollowUp>, ImportError> {
    if plan.data_changed {
        plan.data.save(database).await?;
    }
    plan.patch.write(database, task, plan.now).await?;
    plan.audit.write(database, task, plan.now).await?;
    // PHP only logs a failure here: everything else took effect, and a
    // retry would publish the commit twice.
    let _ = rechecks::queue(database, &plan.rechecks, plan.now).await;
    Ok(plan.close)
}
