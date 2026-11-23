//! `diffusion.diffquery` (`DiffusionDiffQueryConduitAPIMethod`): the
//! changes of the commit which last modified a path, parsed by
//! `ArcanistDiffParser`. For Git, the diff is read inline
//! (`executeInline()`): nothing is written.
//!
//! Ported for Git: every parameter. Left to PHP: diffs which are not
//! UTF-8 (the parser would detect binary files or convert them with the
//! repository's encoding), diffs the Rust parser leaves to PHP, and Git
//! failures.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use phab_diff::arcanist::{self, ParseError};
use serde_json::{Map, Value, json};

use crate::drequest::DiffusionRequest;
use crate::file_future::DEFAULT_TIMEOUT;
use crate::last_modified_query;
use crate::php::Key;
use crate::query::QueryMethod;
use crate::raw_diff_query::{DEFAULT_CONTEXT, RawDiff};

/// `getEffectiveCommit()`: what `diffusion.lastmodifiedquery` answers for
/// the path at the stable commit.
async fn effective_commit(
    request: &ApiRequest<'_>,
    drequest: &DiffusionRequest,
    path: &str,
) -> Result<Value, MethodError> {
    let stable = drequest.stable_commit(request).await?;
    let key = Key::from_text(path);
    let results =
        last_modified_query::last_modified(request, drequest, &[(key.clone(), stable)]).await?;
    Ok(results.get(&key).cloned().unwrap_or(Value::Null))
}

/// `mpull($changes, 'toDictionary')`, keyed like the parser's array.
fn changes(diff: &str) -> Result<Value, MethodError> {
    let keyed = match arcanist::parse_diff_keyed(diff) {
        Ok(keyed) => keyed,
        Err(ParseError::Empty) => return Err(MethodError::Unsupported("diffs of whitespace")),
        Err(ParseError::Unsupported(what)) => return Err(MethodError::Unsupported(what)),
    };
    if keyed.iter().all(|(key, _)| key.is_none()) {
        return Ok(Value::Array(
            keyed
                .iter()
                .map(|(_, change)| change.to_dictionary())
                .collect(),
        ));
    }
    if keyed.iter().any(|(key, _)| key.is_none()) {
        return Err(MethodError::Unsupported(
            "diffs with changes with and without paths",
        ));
    }
    let mut map = Map::new();
    for (key, change) in keyed {
        map.insert(key.unwrap_or_default(), change.to_dictionary());
    }
    Ok(Value::Object(map))
}

/// `diffusion.diffquery`.
pub struct DiffQuery;

impl QueryMethod for DiffQuery {
    fn name(&self) -> &'static str {
        "diffusion.diffquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &["path", "commit", "callsign", "repository", "branch"]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            let path = drequest.path()?;
            let effective = effective_commit(request, drequest, &path).await?;
            let commit = match &effective {
                Value::String(commit) if phab_core::php::is_truthy_str(commit) => commit.clone(),
                Value::Null | Value::String(_) => {
                    return Ok(json!({"changes": [], "effectiveCommit": effective}));
                }
                _ => return Err(MethodError::internal("an unusual last modified commit")),
            };
            let diff = RawDiff {
                commit: &commit,
                against: None,
                lines_of_context: DEFAULT_CONTEXT,
                path: &path,
            };
            let raw = diff.execute_inline(drequest, DEFAULT_TIMEOUT).await?;
            let changes = if raw.is_empty() || raw == b"0" {
                json!([])
            } else {
                let raw = String::from_utf8(raw)
                    .map_err(|_| MethodError::Unsupported("diffs which aren't UTF-8"))?;
                changes(&raw)?
            };
            Ok(json!({"changes": changes, "effectiveCommit": effective}))
        })
    }
}
