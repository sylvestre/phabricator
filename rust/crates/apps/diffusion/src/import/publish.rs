//! `PhabricatorRepositoryCommitPublishWorker`: the last step, for commits
//! the publisher holds (`getCommitHoldReasons()`: the repository is
//! importing or doesn't publish, or the commit is on no permanent ref),
//! which only records the hold reasons and the `IMPORTED_PUBLISH` flag.
//!
//! Publishing a commit is left to PHP: it closes the revisions its message
//! names (`DiffusionLowLevelCommitFieldsQuery`, with Mozilla's
//! `diffusion.legacy-repos-mapping`), applies the commit transaction
//! (`PhabricatorAuditEditor`: Herald, audits, feed, mail, mentions) and
//! queues the merge conflict rechecks.

use phab_core::php;
use phab_db::Database;
use serde_json::json;

use crate::import::commit::{CommitData, ParserTask};
use crate::import::repository::HOLD_NOT_REACHABLE_FROM_PERMANENT_REF;
use crate::import::{IMPORTED_PUBLISH, ImportError};

/// `getCommitHoldReasons()`.
pub fn hold_reasons(task: &ParserTask) -> Vec<&'static str> {
    let mut reasons = task.repository.hold_reasons();
    if !task.commit.is_permanent() {
        reasons.push(HOLD_NOT_REACHABLE_FROM_PERMANENT_REF);
    }
    reasons
}

/// What the step will do.
#[derive(Debug, Clone, PartialEq)]
pub enum Prepared {
    /// The step was done before.
    Skip,
    /// The commit is held: its data and the reasons.
    Hold(CommitData, Vec<&'static str>),
}

/// Checks the step can be done here: the commit must be held, and have
/// its data row (which `DiffusionCommitQuery::needCommitData()` attaches).
pub async fn prepare(database: &Database, task: &ParserTask) -> Result<Prepared, ImportError> {
    if task.should_skip_step(IMPORTED_PUBLISH) {
        return Ok(Prepared::Skip);
    }
    let reasons = hold_reasons(task);
    if reasons.is_empty() {
        return Err(ImportError::unsupported(
            "publishing a commit (Herald, audits, feed, mail and closing revisions)",
        ));
    }
    let Some(data) = CommitData::load_existing(database, task.commit.id).await? else {
        return Err(ImportError::unsupported("a commit without its data"));
    };
    Ok(Prepared::Hold(data, reasons))
}

/// `parseCommit()` of a held commit: the hold reasons are saved when they
/// changed (`!==`), then the flag.
pub async fn parse(
    database: &Database,
    task: &mut ParserTask,
    prepared: Prepared,
) -> Result<(), ImportError> {
    let Prepared::Hold(mut data, reasons) = prepared else {
        return Ok(());
    };
    let reasons = json!(reasons);
    let stored = data.detail("holdReasons").map(php::round_trip);
    if stored.as_ref() != Some(&reasons) {
        data.set_detail("holdReasons", reasons);
        data.save(database).await?;
    }
    task.commit
        .write_import_status_flag(database, IMPORTED_PUBLISH)
        .await?;
    Ok(())
}
