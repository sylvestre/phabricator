//! `diffusion.getrecentcommitsbypath`
//! (`DiffusionGetRecentCommitsByPathConduitAPIMethod`): the latest commits
//! of a Git path, as `r<callsign><hash>` names, through
//! `diffusion.historyquery` at the branch head.
//!
//! Ported for Git: every parameter. Left to PHP: what
//! `diffusion.historyquery` leaves to PHP, and values of unusual types.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::php as core_php;
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;
use crate::history_query::history;

/// `DEFAULT_LIMIT`.
const DEFAULT_LIMIT: i64 = 10;

/// `diffusion.getrecentcommitsbypath`.
pub struct GetRecentCommitsByPath;

impl Method for GetRecentCommitsByPath {
    fn name(&self) -> &'static str {
        "diffusion.getrecentcommitsbypath"
    }

    fn params(&self) -> &'static [&'static str] {
        &["callsign", "path", "branch", "limit"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[("ERR_NOT_FOUND", "Repository was not found.")]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let callsign = request.value("callsign");
            let Some(drequest) =
                DiffusionRequest::load(request, callsign, request.value("path")).await?
            else {
                return Err(MethodError::Conduit {
                    code: "ERR_NOT_FOUND".to_owned(),
                    info: None,
                });
            };
            // nonempty($limit, 10).
            let limit = match request.value("limit") {
                Some(limit) if core_php::is_truthy(limit) => limit.clone(),
                _ => json!(DEFAULT_LIMIT),
            };
            let commit = json!(drequest.commit(request).await?);
            let path = json!(drequest.path()?);
            let result = history(
                request,
                &drequest,
                Some(&commit),
                None,
                Some(&path),
                Some(&json!(0)),
                Some(&limit),
            )
            .await?;
            let callsign = core_php::string_cast(callsign.unwrap_or(&Value::Null)).into_owned();
            let names = result["pathChanges"]
                .as_array()
                .map(|changes| {
                    changes
                        .iter()
                        .map(|change| {
                            json!(format!(
                                "r{callsign}{}",
                                change["commitIdentifier"].as_str().unwrap_or_default()
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(Value::Array(names))
        })
    }
}
