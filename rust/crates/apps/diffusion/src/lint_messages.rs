//! `diffusion.getlintmessages` (`DiffusionGetLintMessagesConduitAPIMethod`):
//! the lint messages stored for files of a repository branch.
//!
//! Ported: every parameter. Left to PHP: repositories the viewer can't
//! see (`executeOne()` raises a policy exception), an empty `files` list
//! (`qsprintf()` refuses it), and values of unusual types.

use phab_app_differential::repository;
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture, pager};
use phab_core::{Phid, php as core_php};
use phab_db::rows;
use serde_json::{Value, json};

/// `diffusion.getlintmessages`.
pub struct GetLintMessages;

impl Method for GetLintMessages {
    fn name(&self) -> &'static str {
        "diffusion.getlintmessages"
    }

    fn params(&self) -> &'static [&'static str] {
        &["repositoryPHID", "branch", "commit", "files"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let Some(Value::String(phid)) = request.value("repositoryPHID") else {
                return Err(MethodError::Unsupported(
                    "a repository PHID which is not a string",
                ));
            };
            let context = request.context;
            let database = &context.database;
            let found = repository::load(database, &[Phid::new(phid.clone())]).await?;
            if found.is_empty() {
                return Err(pager::core_error(format!(
                    "No repository exists with PHID \"{phid}\"."
                )));
            }
            let visible = repository::visible(
                database,
                &context.config,
                &request.viewer,
                found,
                request.now,
            )
            .await
            .map_err(MethodError::internal)?;
            let Some(repository) = visible.into_iter().next() else {
                return Err(MethodError::Unsupported(
                    "a repository the viewer can't see",
                ));
            };
            // $branch_name == '': null, false and the empty string.
            let branch = match request.value("branch") {
                None | Some(Value::Null | Value::Bool(false)) => None,
                Some(Value::String(branch)) if branch.is_empty() => None,
                Some(Value::String(branch)) => Some(branch.clone()),
                Some(_) => return Err(MethodError::Unsupported("a branch which is not a string")),
            };
            // getDefaultArcanistBranch().
            let branch = branch.unwrap_or_else(|| {
                repository
                    .default_branch()
                    .unwrap_or_else(|| "svn".to_owned())
            });
            let files: Vec<String> = match request.value("files") {
                Some(Value::Array(files)) if !files.is_empty() => files
                    .iter()
                    .map(|file| file.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .ok_or(MethodError::Unsupported("files which are not strings"))?,
                _ => return Err(MethodError::Unsupported("files which are not a list")),
            };

            let sql = format!(
                "SELECT id, lintCommit FROM {} WHERE repositoryID = ? AND name = ? LIMIT 2",
                database.table("repository", "repository_branch")
            );
            let branches = phab_db::sqlx::query(&sql)
                .bind(repository.id)
                .bind(&branch)
                .fetch_all(database.pool())
                .await?;
            let row = match branches.as_slice() {
                [] => return Ok(json!([])),
                [row] => row,
                _ => {
                    return Err(MethodError::Unsupported(
                        "several branches of the same name",
                    ));
                }
            };
            let lint_commit = rows::opt_text(row, "lintCommit")?;
            if !lint_commit.as_deref().is_some_and(core_php::is_truthy_str) {
                return Ok(json!([]));
            }
            let sql = format!(
                "SELECT path, line, code FROM {} WHERE branchID = ? AND path IN ({})",
                database.table("repository", "repository_lintmessage"),
                phab_db::placeholders(files.len())
            );
            let messages = phab_db::bind_all(
                phab_db::sqlx::query(&sql).bind(rows::int(row, "id")?),
                files.iter(),
            )
            .fetch_all(database.pool())
            .await?;
            let mut results = Vec::new();
            for message in &messages {
                results.push(json!({
                    "path": rows::text(message, "path")?,
                    "line": rows::int(message, "line")?.to_string(),
                    "code": rows::text(message, "code")?,
                }));
            }
            Ok(Value::Array(results))
        })
    }
}
