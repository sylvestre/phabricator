//! `differential.getcommitpaths` (`DifferentialGetCommitPathsConduitAPIMethod`):
//! the paths the latest diff of a revision touches.

use phab_conduit::pager::{self, Direction};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_db::rows;
use serde_json::{Value, json};

use crate::search::{self, Constraints, Order, Revisions};

/// `differential.getcommitpaths`.
pub struct GetCommitPaths;

impl Method for GetCommitPaths {
    fn name(&self) -> &'static str {
        "differential.getcommitpaths"
    }

    fn params(&self) -> &'static [&'static str] {
        &["revision_id"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let id = match request.value("revision_id") {
                Some(Value::Number(number)) if !number.is_f64() => number.as_i64(),
                Some(Value::String(text))
                    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    text.parse().ok()
                }
                _ => None,
            }
            .ok_or(MethodError::Unsupported("an unusual revision ID"))?;
            let query = Revisions {
                request,
                constraints: Constraints {
                    ids: Some(vec![id]),
                    ..Constraints::default()
                },
                order: Order::Updated,
                memory: std::sync::Mutex::default(),
            };
            if pager::execute(&query, None, Direction::Forward, 1)
                .await?
                .is_empty()
            {
                // executeOne() raises a policy exception for a revision the
                // viewer can't see.
                let sql = format!(
                    "SELECT 1 FROM {} WHERE id = ?",
                    request
                        .context
                        .database
                        .table("differential", "differential_revision")
                );
                if phab_db::sqlx::query(&sql)
                    .bind(id)
                    .fetch_optional(request.context.database.pool())
                    .await?
                    .is_some()
                {
                    return Err(MethodError::Unsupported("a revision the viewer can't see"));
                }
                return Err(MethodError::Conduit {
                    code: "ERR_NOT_FOUND".to_owned(),
                    info: Some("No such revision exists.".to_owned()),
                });
            }
            // The latest diff, whatever its policy.
            let database = &request.context.database;
            let sql = format!(
                "SELECT id FROM {} WHERE revisionID = ? ORDER BY id DESC LIMIT 1",
                database.table("differential", "differential_diff")
            );
            let diff = phab_db::sqlx::query(&sql)
                .bind(id)
                .fetch_optional(database.pool())
                .await?
                // PHP calls a method on null.
                .ok_or(MethodError::Unsupported("revisions without a diff"))?;
            let sql = format!(
                "SELECT filename FROM {} WHERE diffID = ? ORDER BY id",
                database.table("differential", "differential_changeset")
            );
            let paths = phab_db::sqlx::query(&sql)
                .bind(rows::int(&diff, "id")?)
                .fetch_all(database.pool())
                .await?
                .iter()
                .map(|row| rows::text(row, "filename"))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!(paths))
        })
    }
}
