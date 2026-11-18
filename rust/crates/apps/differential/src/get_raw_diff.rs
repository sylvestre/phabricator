//! `differential.getrawdiff` (`DifferentialGetRawDiffConduitAPIMethod`):
//! a diff as a git patch (`DifferentialRawDiffRenderer`).

use phab_conduit::pager::Direction;
use phab_conduit::search::IdOrder;
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_db::rows;
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
            let found = diff_search::query_diffs(
                request,
                Some(vec![id]),
                None,
                IdOrder::Newest,
                Direction::Forward,
            )
            .await?;
            if found.is_empty() {
                // willFilterPage() drops a diff whose revision the viewer
                // can't see, but executeOne() raises a policy exception for
                // a diff without a revision.
                let database = &request.context.database;
                let sql = format!(
                    "SELECT revisionID FROM {} WHERE id = ?",
                    database.table("differential", "differential_diff")
                );
                if let Some(row) = phab_db::sqlx::query(&sql)
                    .bind(id)
                    .fetch_optional(database.pool())
                    .await?
                    && rows::opt_int(&row, "revisionID")?.unwrap_or(0) == 0
                {
                    return Err(MethodError::Unsupported("a diff the viewer can't see"));
                }
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
