//! `diffusion.historyquery` (`DiffusionHistoryQueryConduitAPIMethod`): the
//! commits of a Git path (`git log --format=%H:%P`), with their parents.
//!
//! Ported for Git: every parameter (`needDirectChanges` and
//! `needChildChanges` only matter to Subversion). Left to PHP: values of
//! unusual types.

use phab_conduit::{ApiRequest, MethodError, MethodFuture};
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;
use crate::php::{self, Array, Key};
use crate::query::QueryMethod;
use crate::{git, history};

/// The history of `path` at `commit`: the path changes and the parents of
/// each commit.
pub async fn history(
    request: &ApiRequest<'_>,
    drequest: &DiffusionRequest,
    commit: Option<&Value>,
    against: Option<&Value>,
    path: Option<&Value>,
    offset: Option<&Value>,
    limit: Option<&Value>,
) -> Result<Value, MethodError> {
    let commit = php::optional_string(commit)?;
    let range = match php::nonempty_string(against)? {
        Some(against) => format!("{against}..{}", commit.unwrap_or_default()),
        None => commit.ok_or(MethodError::Unsupported("a missing commit"))?,
    };
    let path = php::nonempty_string(path)?;
    let mut argv = vec![
        "log".to_owned(),
        "--skip".to_owned(),
        php::shell_string(
            offset
                .filter(|offset| !offset.is_null())
                .or(Some(&json!(0))),
        )?,
        "--max-count".to_owned(),
        php::shell_string(limit)?,
        "--format=%H:%P".to_owned(),
        git::ref_selector(&range)?,
        "--".to_owned(),
    ];
    argv.extend(path);
    let stdout = php::text(drequest.copy.execx(argv, None).await?)?;
    let mut hashes = Vec::new();
    let mut parents = Array::new();
    for line in php::trim(&stdout)
        .split('\n')
        .filter(|line| phab_core::php::is_truthy_str(line))
    {
        let Some((hash, rest)) = line.split_once(':') else {
            return Err(MethodError::Unsupported("an unusual history line"));
        };
        // list($hash, $parents) = explode(':', $line) ignores later parts.
        let rest = rest.split(':').next().unwrap_or_default();
        hashes.push(hash.to_owned());
        parents.set(Key::from_text(hash), json!(php::split_whitespace(rest, 0)));
    }
    let changes = history::load_history(&request.context.database, drequest, &hashes).await?;
    Ok(json!({
        "pathChanges": changes,
        "parents": parents.into_json(),
    }))
}

/// `diffusion.historyquery`.
pub struct HistoryQuery;

impl QueryMethod for HistoryQuery {
    fn name(&self) -> &'static str {
        "diffusion.historyquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "commit",
            "against",
            "path",
            "offset",
            "limit",
            "needDirectChanges",
            "needChildChanges",
            "callsign",
            "repository",
            "branch",
        ]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            history(
                request,
                drequest,
                request.value("commit"),
                request.value("against"),
                request.value("path"),
                request.value("offset"),
                request.value("limit"),
            )
            .await
        })
    }
}
