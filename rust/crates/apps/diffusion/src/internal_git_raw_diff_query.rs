//! `diffusion.internal.gitrawdiffquery`
//! (`DiffusionInternalGitRawDiffQueryConduitAPIMethod`): the `--raw` diff
//! of a Git commit against its first parent, as the commit parser reads
//! it.
//!
//! Ported for Git: every parameter. Left to PHP: Git failures, and values
//! of unusual types.

use std::time::Duration;

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use serde_json::json;

use crate::drequest::DiffusionRequest;
use crate::git::{self, WorkingCopy};
use crate::php;
use crate::query::QueryMethod;

/// How long the slower `--find-copies-harder` diff may take.
const HARDER_TIMEOUT: Duration = Duration::from_secs(30);

fn raw_diff_argv(
    commit: &str,
    use_log: bool,
    try_harder: bool,
) -> Result<Vec<String>, MethodError> {
    let mut flags: Vec<String> = ["-n1", "-M", "-C", "-B", "--raw", "-t", "--abbrev=40"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    if try_harder {
        flags.push("--find-copies-harder".to_owned());
    }
    Ok(if use_log {
        // The first commit: "--pretty=format: " leaves only the raw part.
        let mut argv = vec!["log".to_owned()];
        argv.extend(flags);
        argv.extend([
            "--pretty=format:".to_owned(),
            git::ref_selector(commit)?,
            "--".to_owned(),
        ]);
        argv
    } else {
        let mut argv = vec!["diff".to_owned()];
        argv.extend(flags);
        argv.extend([
            git::ref_selector(&format!("{commit}^1"))?,
            git::ref_selector(commit)?,
            "--".to_owned(),
        ]);
        argv
    })
}

/// `getGitResult()`: the fast diff, then the better one for small
/// changes when Git gives it in time.
pub async fn raw_diff(copy: &WorkingCopy, commit: &str) -> Result<String, MethodError> {
    let parents = copy
        .execx(
            vec![
                "log".to_owned(),
                "-n1".to_owned(),
                "--format=%P".to_owned(),
                git::ref_selector(commit)?,
                "--".to_owned(),
            ],
            None,
        )
        .await?;
    let use_log = php::trim(&php::text(parents)?).is_empty();
    let raw = php::text(
        copy.execx(raw_diff_argv(commit, use_log, false)?, None)
            .await?,
    )?;
    if raw.matches('\n').count() > 100 {
        return Ok(raw);
    }
    // PHP ignores any failure of the slower diff, a timeout included.
    match copy
        .exec_with_timeout(raw_diff_argv(commit, use_log, true)?, None, HARDER_TIMEOUT)
        .await
    {
        Ok(output) if output.success() => php::text(output.stdout),
        Ok(_) | Err(MethodError::Unsupported("a Git command which timed out")) => Ok(raw),
        Err(error) => Err(error),
    }
}

/// `diffusion.internal.gitrawdiffquery`.
pub struct InternalGitRawDiffQuery;

impl QueryMethod for InternalGitRawDiffQuery {
    fn name(&self) -> &'static str {
        "diffusion.internal.gitrawdiffquery"
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
            let commit = php::string(request.value("commit"))?;
            Ok(json!(raw_diff(&drequest.copy, &commit).await?))
        })
    }
}
