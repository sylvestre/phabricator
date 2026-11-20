//! "Depends on D123" in a revision's summary or test plan
//! (`expandCustomRemarkupBlockTransactions()`): the "depends on" edge to
//! each revision it names.

use phab_conduit::{ApiRequest, MethodError};
use phab_core::Phid;
use phab_db::edges;

use super::Link;
use crate::storage::Revision;

/// `(int)trim($monogram, 'dD')`.
fn id(monogram: &[u8]) -> i64 {
    monogram[1..]
        .iter()
        .try_fold(0_i64, |id, digit| {
            id.checked_mul(10)?.checked_add(i64::from(digit - b'0'))
        })
        .unwrap_or(i64::MAX)
}

/// `DifferentialCustomFieldDependsOnParser::parseCorpus()`: the revision
/// IDs named, in order.
pub(super) fn depends_on(corpus: &str) -> Vec<i64> {
    let mut ids = Vec::new();
    for monogram in super::monograms::DEPENDS_ON.parse(corpus) {
        let id = id(monogram.as_bytes());
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// Whether `from` depends on `to`, directly or not.
pub(super) async fn depends(
    request: &ApiRequest<'_>,
    from: &Phid,
    to: &Phid,
) -> Result<bool, MethodError> {
    let database = &request.context.database;
    let mut seen: Vec<Phid> = vec![from.clone()];
    let mut frontier = vec![from.clone()];
    while !frontier.is_empty() {
        let next: Vec<Phid> = edges::load_edges(
            database,
            &frontier,
            &[edges::REVISION_DEPENDS_ON_REVISION],
            None,
        )
        .await?
        .into_iter()
        .map(|edge| edge.dst)
        .filter(|dst| !seen.contains(dst))
        .collect();
        if next.contains(to) {
            return Ok(true);
        }
        seen.extend(next.iter().cloned());
        frontier = next;
    }
    Ok(false)
}

/// The dependencies `text` adds, and the revisions it names, which the edit
/// doesn't mention. Cycles are left to PHP (PHP refuses them).
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    revision: &Revision,
    text: &str,
) -> Result<(Option<Link>, Vec<String>), MethodError> {
    let ids = depends_on(text);
    if ids.is_empty() {
        return Ok((None, Vec::new()));
    }
    let found = super::other_revisions(request, revision, ids).await?;
    let named: Vec<String> = found
        .iter()
        .map(|found| found.phid.as_str().to_owned())
        .collect();
    for target in &found {
        if depends(request, &target.phid, &revision.phid).await? {
            return Err(MethodError::Unsupported("dependency cycles"));
        }
    }
    let edge_type = edges::REVISION_DEPENDS_ON_REVISION;
    let link = super::plan_link(request, revision, edge_type, found).await?;
    Ok((link, named))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_php() {
        assert_eq!(depends_on("Fix it.\n\nDepends on D7"), [7]);
        assert_eq!(depends_on("depends on D1, D2 and d3."), [1, 2, 3]);
        assert_eq!(depends_on("Depends on revision D12"), [12]);
        assert_eq!(depends_on("Depends on diffs D4 D5"), [4, 5]);
        // No boundary, no space, or a word after the digits.
        for text in [
            "Undepends on D1",
            "Depends onD1",
            "Depends on D1x",
            "Depends on nothing; @bob knows this code.",
        ] {
            assert_eq!(depends_on(text), Vec::<i64>::new(), "{text}");
        }
    }
}
