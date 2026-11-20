//! "Reverts D123" or "Backs out commit abcdef" in a revision's summary or
//! test plan (`expandCustomRemarkupBlockTransactions()`): the "reverts"
//! edge to each object it names
//! (`DiffusionCommitRevisionQuery::loadRevertedObjects()`).

use phab_conduit::{ApiRequest, MethodError};
use phab_core::Phid;
use phab_db::edges;

use super::{Link, monograms};
use crate::storage::Revision;

/// `DiffusionCommitRevertsCommitEdgeType`.
pub(super) const REVERTS: i64 = 55;
/// `DiffusionCommitRevertedByCommitEdgeType`.
pub(super) const REVERTED_BY: i64 = 56;

/// A revision's name (`DifferentialRevisionPHIDType::canLoadNamedObject()`,
/// `^D\d*[1-9]\d*$`): its ID.
fn revision_id(monogram: &str) -> Option<i64> {
    let digits = monogram.strip_prefix('D')?;
    if digits.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || digits.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    Some(digits.parse().unwrap_or(i64::MAX))
}

/// The reverts `text` adds, and the objects it names, which the edit
/// doesn't mention: commits first, newest first, then revisions in the
/// order of `DifferentialRevisionQuery`. Reverting a commit with a revision
/// (which PHP reverts too) is left to PHP.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    revision: &Revision,
    text: &str,
) -> Result<(Option<Link>, Vec<String>), MethodError> {
    let names = monograms::REVERTS.parse(text);
    if names.is_empty() {
        return Ok((None, Vec::new()));
    }
    let ids: Vec<i64> = names.iter().filter_map(|name| revision_id(name)).collect();
    // Every name is a commit identifier too.
    let commits = super::commits::load(request, &names).await?;
    let revisions = super::other_revisions(request, revision, ids).await?;
    let database = &request.context.database;
    let phids: Vec<Phid> = commits
        .iter()
        .map(|commit| Phid::new(commit.phid.as_str()))
        .collect();
    if !phids.is_empty()
        && !edges::load_edges(database, &phids, &[edges::COMMIT_HAS_REVISION], None)
            .await?
            .is_empty()
    {
        return Err(MethodError::Unsupported(
            "reverts of commits with revisions",
        ));
    }
    let mut reverted: Vec<String> = commits.iter().map(|commit| commit.phid.clone()).collect();
    reverted.extend(revisions.iter().map(|found| found.phid.as_str().to_owned()));
    let old = super::destinations(database, &revision.phid, REVERTS).await?;
    let mut targets = Vec::new();
    for commit in commits {
        if !old.contains(&commit.phid) {
            targets.push(super::commit_target(commit));
        }
    }
    for found in revisions {
        if !old.iter().any(|phid| phid == found.phid.as_str()) {
            targets.push(super::plan_target(request, found, true).await?);
        }
    }
    Ok((super::new_link(REVERTS, targets), reverted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_revisions_like_php() {
        assert_eq!(revision_id("D12"), Some(12));
        for name in ["d12", "D", "D0", "D00", "Dab", "a"] {
            assert_eq!(revision_id(name), None, "{name}");
        }
    }
}
