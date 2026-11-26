//! Mozilla's AI change on the revision page (Bug 2091563): the "Approve AI
//! Change" comment action, offered as PHP's comment form does
//! (`DifferentialRevisionAIChangeApproveTransaction::newEditField()`): one
//! checkbox for the pending diff, which starts unticked.

use phab_app_differential::ai_change::{AiChange, Status};
use phab_app_differential::storage::Revision;
use phab_auth::User;
use phab_db::{Database, rows};

use crate::error::PageError;

/// The AI change of `revision`, from its `properties`.
pub(crate) async fn load(database: &Database, revision: &Revision) -> Result<AiChange, PageError> {
    let sql = format!(
        "SELECT properties FROM {} WHERE id = ?",
        database.table("differential", "differential_revision")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(revision.id)
        .fetch_optional(database.pool())
        .await
        .map_err(crate::harbormaster::internal)?
        .ok_or(PageError::NotFound)?;
    let properties = rows::json(&row, "properties").map_err(crate::harbormaster::internal)?;
    AiChange::from_properties(&properties).map_err(crate::harbormaster::method_error)
}

/// `isActionAvailable()` for `viewer`: a pending change on an open
/// revision, and a viewer who isn't a bot or a mailing list.
pub(crate) fn can_approve(change: &AiChange, revision: &Revision, viewer: &User) -> bool {
    let active = revision
        .active_diff_phid
        .as_ref()
        .map(phab_core::Phid::as_str);
    !viewer.is_system_agent
        && !viewer.is_mailing_list
        && !revision.is_closed()
        && change.status(active) == Status::Pending
}

/// The diff the checkbox approves, when the viewer may approve: the stored
/// `changeDiffID`, an integer.
pub(crate) fn approvable_diff(
    change: &AiChange,
    revision: &Revision,
    viewer: &User,
) -> Option<i64> {
    if !can_approve(change, revision, viewer) {
        return None;
    }
    change
        .get(phab_app_differential::ai_change::KEY_CHANGE_DIFF_ID)
        .and_then(serde_json::Value::as_i64)
}
