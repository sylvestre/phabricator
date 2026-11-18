//! `differential.getrawdiff` (`DifferentialGetRawDiffConduitAPIMethod`):
//! a diff as a git patch (`DifferentialRawDiffRenderer`).

use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_diff::git_patch;
use phab_diff::storage::{self as diff_storage, StorageError};
use serde_json::{Value, json};

use crate::diff_search;

/// `differential.getrawdiff`.
pub struct GetRawDiff;

impl Method for GetRawDiff {
    fn name(&self) -> &'static str {
        "differential.getrawdiff"
    }

    fn params(&self) -> &'static [&'static str] {
        &["diffID"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(crate::search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let id = match request.value("diffID") {
                Some(Value::Number(number)) if !number.is_f64() => number.as_i64(),
                Some(Value::String(text))
                    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    text.parse().ok()
                }
                _ => None,
            }
            .ok_or(MethodError::Unsupported("an unusual diff ID"))?;
            if diff_search::execute_one(request, id).await?.is_none() {
                return Err(MethodError::Conduit {
                    code: "ERR_NOT_FOUND".to_owned(),
                    info: Some("Diff not found.".to_owned()),
                });
            }
            let changesets = diff_storage::load_changesets(&request.context.database, id)
                .await
                .map_err(|error| match error {
                    StorageError::Database(error) => MethodError::internal(error),
                    _ => MethodError::Unsupported("hunks Rust can't read"),
                })?;
            let patch = git_patch::to_git_patch(&changesets)
                .map_err(|_| MethodError::Unsupported("binary changes and multicopies"))?;
            Ok(json!(patch))
        })
    }
}
