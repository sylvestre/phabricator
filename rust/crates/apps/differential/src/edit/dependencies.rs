//! "Depends on D123" in a revision's summary or test plan
//! (`expandCustomRemarkupBlockTransactions()`): the "depends on" edge to
//! each revision it names.

use phab_conduit::{ApiRequest, MethodError};
use phab_core::Phid;
use phab_db::edges;

use super::Link;
use crate::storage::Revision;

/// The infixes `DifferentialCustomFieldDependsOnParser` allows between
/// "depends on" and the monograms.
const INFIXES: &[&str] = &[
    "diff", "diffs", "change", "changes", "rev", "revs", "revision",
];

/// `\w` without the `u` flag.
fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `\s` without the `u` flag.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\x0B' | b'\x0C' | b'\r')
}

/// Where a run of `\s` from `at` ends.
fn skip_spaces(text: &[u8], mut at: usize) -> usize {
    while text.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    at
}

/// `[Dd]\d+(?:\b|$)` at `at`: where it ends.
fn monogram(text: &[u8], at: usize) -> Option<usize> {
    if !matches!(text.get(at), Some(b'D' | b'd')) {
        return None;
    }
    let mut end = at + 1;
    while text.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    let bounded = !text.get(end).copied().is_some_and(is_word);
    (end > at + 1 && bounded).then_some(end)
}

/// The monograms of a match starting at `at`, after "depends on", and
/// where the match ends.
fn monograms_after(text: &[u8], lower: &[u8], at: usize) -> Option<(Vec<i64>, usize)> {
    let after = skip_spaces(text, at);
    if after == at {
        return None;
    }
    // An optional infix, followed by spaces, before the first monogram.
    let start = INFIXES
        .iter()
        .filter(|infix| lower[after..].starts_with(infix.as_bytes()))
        .map(|infix| after + infix.len())
        .map(|end| (end, skip_spaces(text, end)))
        .find(|&(end, next)| next > end && monogram(text, next).is_some())
        .map_or(after, |(_, next)| next);
    let mut found = Vec::new();
    let mut at = start;
    // `(?:MONOGRAM(?:\b|$)[,\s]*)+`
    while let Some(end) = monogram(text, at) {
        found.push(id(&text[at..end]));
        at = end;
        while text
            .get(at)
            .is_some_and(|byte| *byte == b',' || is_space(*byte))
        {
            at += 1;
        }
    }
    if found.is_empty() {
        return None;
    }
    // `(?:\band\s+(MONOGRAM(?:\b|$)))?`
    if lower[at..].starts_with(b"and") && at > 0 && !is_word(text[at - 1]) {
        let next = skip_spaces(text, at + 3);
        if next > at + 3
            && let Some(end) = monogram(text, next)
        {
            found.push(id(&text[next..end]));
            at = end;
        }
    }
    Some((found, at))
}

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
    let text = corpus.as_bytes();
    let lower = corpus.to_ascii_lowercase().into_bytes();
    let mut ids = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let bounded = at == 0 || !is_word(text[at - 1]);
        if bounded
            && lower[at..].starts_with(b"depends on")
            && let Some((found, end)) = monograms_after(text, &lower, at + "depends on".len())
        {
            for id in found {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            at = end;
            continue;
        }
        at += 1;
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
