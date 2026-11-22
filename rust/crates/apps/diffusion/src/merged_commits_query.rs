//! `diffusion.mergedcommitsquery` (`DiffusionMergedCommitsQueryConduitAPIMethod`):
//! the commits a Git merge brings in, which its first parent can't reach.
//!
//! Ported for Git: every parameter. Left to PHP: values of unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use serde_json::Value;

use crate::commit_parents_query::parents;
use crate::drequest::DiffusionRequest;
use crate::query::QueryMethod;
use crate::{git, history, php};

/// `diffusion.mergedcommitsquery`.
pub struct MergedCommitsQuery;

impl QueryMethod for MergedCommitsQuery {
    fn name(&self) -> &'static str {
        "diffusion.mergedcommitsquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &["commit", "limit", "callsign", "repository", "branch"]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            let commit = php::string(request.value("commit"))?;
            let limit = php::int_param(request.value("limit"), 4096)?;
            let parents = parents(&drequest.copy, &commit).await?;
            let [first_parent, _, ..] = parents.as_slice() else {
                // Not a merge commit.
                return Ok(Value::Array(Vec::new()));
            };
            let stdout = drequest
                .copy
                .execx(
                    vec![
                        "log".to_owned(),
                        "-n".to_owned(),
                        // "+ 1" for the merge commit itself.
                        limit
                            .checked_add(1)
                            .ok_or(MethodError::Unsupported("a huge limit"))?
                            .to_string(),
                        "--format=%H".to_owned(),
                        git::ref_selector(&commit)?,
                        git::ref_selector(&format!("^{first_parent}"))?,
                        "--".to_owned(),
                    ],
                    None,
                )
                .await?;
            let stdout = php::text(stdout)?;
            let hashes: Vec<String> = php::trim(&stdout)
                .split('\n')
                .filter(|hash| *hash != commit)
                .map(str::to_owned)
                .collect();
            let history =
                history::load_history(&request.context.database, drequest, &hashes).await?;
            Ok(Value::Array(history))
        })
    }
}
