//! `diffusion.commitparentsquery` (`DiffusionCommitParentsQueryConduitAPIMethod`
//! with `DiffusionLowLevelParentsQuery`): the parents of a Git commit.
//!
//! Ported for Git: every parameter. Left to PHP: a missing commit (PHP
//! raises an invalid state exception) and values of unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;
use crate::git::{self, WorkingCopy};
use crate::php;
use crate::query::QueryMethod;

/// `git log -n 1 --format=%P <commit> --`, split as `preg_split('/\s+/',
/// trim(...))` splits it.
pub async fn parents(copy: &WorkingCopy, commit: &str) -> Result<Vec<String>, MethodError> {
    let stdout = copy
        .execx(
            vec![
                "log".to_owned(),
                "-n".to_owned(),
                "1".to_owned(),
                "--format=%P".to_owned(),
                git::ref_selector(commit)?,
                "--".to_owned(),
            ],
            None,
        )
        .await?;
    let stdout = php::text(stdout)?;
    Ok(php::split_whitespace(php::trim(&stdout), 0)
        .into_iter()
        .map(str::to_owned)
        .collect())
}

/// `diffusion.commitparentsquery`.
pub struct CommitParentsQuery;

impl QueryMethod for CommitParentsQuery {
    fn name(&self) -> &'static str {
        "diffusion.commitparentsquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &["commit", "callsign", "repository", "branch"]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            let commit = php::nonempty_string(request.value("commit"))?
                .ok_or(MethodError::Unsupported("a missing commit"))?;
            let parents = parents(&drequest.copy, &commit).await?;
            Ok(Value::Array(
                parents.into_iter().map(|hash| json!(hash)).collect(),
            ))
        })
    }
}
