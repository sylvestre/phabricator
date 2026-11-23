//! `parents.add`, `.remove` and `.set`, and the same for `children`: the
//! "depends on" edges from the revision (5) or to it (6), with the inverse
//! edit of each revision they link.

use phab_conduit::{ApiRequest, MethodError};
use phab_db::edges;

use super::Link;
use super::lists::{self, Wanted};
use crate::search::Constraints;
use crate::storage::Revision;

/// The edges of `edge_type` the edit adds and removes, which need
/// `CAN_EDIT`. Revisions the actor can't see, the revision itself and
/// cycles (PHP refuses them) are left to PHP.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    revision: &Revision,
    edge_type: i64,
    wanted: &Wanted,
) -> Result<Link, MethodError> {
    let context = request.context;
    let database = &context.database;
    let old = super::destinations(database, &revision.phid, edge_type).await?;
    let new = lists::new_list(&old, wanted);
    let (added, removed) = (lists::minus(&new, &old), lists::minus(&old, &new));
    if added.is_empty() && removed.is_empty() {
        return Err(MethodError::Unsupported(super::NO_EFFECT));
    }
    if added.iter().any(|phid| phid == revision.phid.as_str()) {
        return Err(MethodError::Unsupported(
            "revisions depending on themselves",
        ));
    }
    let can_edit = crate::storage::can_edit(
        database,
        &context.config,
        &request.viewer,
        revision,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if !can_edit {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    let mut phids = added.clone();
    phids.extend(removed.iter().cloned());
    let constraints = Constraints {
        phids: Some(phids),
        ..Constraints::default()
    };
    let found = super::visible_revisions(request, revision, constraints).await?;
    let visible = |phid: &String| found.iter().any(|found| found.phid.as_str() == phid);
    if !added.iter().all(visible) {
        return Err(MethodError::Unsupported(
            "links to revisions the actor can't see",
        ));
    }
    let mut targets = Vec::new();
    for target in found {
        let is_added = added.iter().any(|phid| phid == target.phid.as_str());
        if is_added {
            let cycle = if edge_type == edges::REVISION_DEPENDS_ON_REVISION {
                super::dependencies::depends(request, &target.phid, &revision.phid).await?
            } else {
                super::dependencies::depends(request, &revision.phid, &target.phid).await?
            };
            if cycle {
                return Err(MethodError::Unsupported("dependency cycles"));
            }
        }
        targets.push(super::plan_target(request, target, is_added).await?);
    }
    Ok(Link {
        edge_type,
        phid: super::transaction_phid(),
        removed,
        added,
        targets,
    })
}
