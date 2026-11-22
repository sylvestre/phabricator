//! `differential.createrawdiff` (`DifferentialCreateRawDiffConduitAPIMethod`):
//! a diff from the text of a Git diff.
//!
//! Ported: Git diffs ([`phab_diff::arcanist::parse_diff`]) and
//! `repositoryPHID`. Left to PHP: other diff formats and the diffs
//! arcanist's parser rejects, `viewPolicy`, and what
//! [`crate::diff_editor`] leaves to PHP.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::php;
use phab_diff::arcanist::{self, ParseError};
use serde_json::{Value, json};

use crate::diff_editor::{self, DiffFields, PlannedDiff};

/// How many changesets and hunks PHP accepts.
const RAW_LIMIT: usize = 10_000;

/// A generic PHP exception, which Conduit reports as `ERR-CONDUIT-CORE`.
fn exception(message: String) -> MethodError {
    MethodError::Conduit {
        code: "ERR-CONDUIT-CORE".to_owned(),
        info: Some(message),
    }
}

/// `differential.createrawdiff`.
pub struct CreateRawDiff;

impl CreateRawDiff {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let actor = diff_editor::actor_phid(request)?;
        let Some(Value::String(raw_diff)) = request.value("diff") else {
            return Err(MethodError::Unsupported("raw diffs which are not strings"));
        };
        let repository_phid = match request.value("repositoryPHID") {
            None | Some(Value::Null) => None,
            Some(Value::String(phid)) => Some(phid.clone()),
            Some(_) => {
                return Err(MethodError::Unsupported(
                    "repository PHIDs which are not strings",
                ));
            }
        };
        if let Some(phid) = repository_phid
            .as_deref()
            .filter(|phid| php::is_truthy_str(phid))
            && diff_editor::repository(request, phid).await?.is_none()
        {
            return Err(exception(format!("No such repository \"{phid}\"!")));
        }
        if repository_phid.as_ref().is_some_and(|phid| phid.len() > 64) {
            return Err(MethodError::Unsupported("values longer than their column"));
        }
        let changes = match arcanist::parse_diff(raw_diff) {
            Ok(changes) => changes,
            Err(ParseError::Empty) => {
                return Err(exception("Can't parse an empty diff!".to_owned()));
            }
            Err(ParseError::Unsupported(what)) => return Err(MethodError::Unsupported(what)),
        };
        let (changesets, line_count) = diff_editor::build_changesets(request, &changes).await?;
        let size: usize = changesets
            .iter()
            .map(|changeset| 1 + changeset.hunks.len())
            .sum();
        if size > RAW_LIMIT {
            return Err(exception(
                "The raw diff you have submitted is too large to parse (it affects more than \
                 10,000 paths and hunks)."
                    .to_owned(),
            ));
        }
        if request.value("viewPolicy").is_some_and(php::is_truthy) {
            return Err(MethodError::Unsupported("raw diffs with a view policy"));
        }
        diff_editor::check_herald(request).await?;
        let planned = PlannedDiff {
            changesets,
            line_count,
            fields: DiffFields {
                creation_method: Some("web".to_owned()),
                author_phid: actor,
                repository_phid,
                lint_status: diff_editor::STATUS_SKIP,
                unit_status: diff_editor::STATUS_SKIP,
                ..DiffFields::default()
            },
            properties: Vec::new(),
        };
        let (id, phid) = diff_editor::save(request, &planned).await?;
        let uri = format!(
            "{}/differential/diff/{id}/",
            production_uri(request)?.trim_end_matches('/')
        );
        Ok(json!({"id": id, "phid": phid, "uri": uri}))
    }
}

/// `PhabricatorEnv::getProductionURI()`'s base.
fn production_uri(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    let config = &request.context.config;
    match config.nonempty_string("phabricator.production-uri") {
        Some(uri) => Ok(uri.to_owned()),
        None => Ok(config.base_uri()?.to_owned()),
    }
}

impl Method for CreateRawDiff {
    fn name(&self) -> &'static str {
        "differential.createrawdiff"
    }

    fn params(&self) -> &'static [&'static str] {
        &["diff", "repositoryPHID", "viewPolicy"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}
