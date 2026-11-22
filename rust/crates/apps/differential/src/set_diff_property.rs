//! `differential.setdiffproperty`
//! (`DifferentialSetDiffPropertyConduitAPIMethod`): a JSON property of a
//! diff, saved as a plain `differential_diffproperty` row.
//!
//! Ported: every parameter. Left to PHP: values which are not strings,
//! JSON PHP and serde may read differently (scalars, deep nesting, lone
//! surrogates, floats), diffs hidden by a policy exception, and revisions
//! the viewer can see but not edit (a policy exception too).

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_db::rows;
use serde_json::Value;

use crate::diff_editor;

const ERR_NOT_FOUND: &str = "ERR_NOT_FOUND";
const ERR_PERMISSIONS: &str = "ERR_PERMISSIONS";
const ERR_INVALID_DATA: &str = "ERR_INVALID_DATA";
const ERR_NO_NAME: &str = "ERR_NO_NAME";

fn conduit_error(code: &str) -> MethodError {
    MethodError::Conduit {
        code: code.to_owned(),
        info: None,
    }
}

/// `phutil_json_decode()` of the data: an array, or `ERR_INVALID_DATA`.
fn decode(data: &str) -> Result<Value, MethodError> {
    match serde_json::from_str::<Value>(data) {
        Ok(value @ (Value::Array(_) | Value::Object(_))) => Ok(value),
        // PHP hands scalars to PhutilJSONParser, which is not ported.
        Ok(_) => Err(MethodError::Unsupported("properties which are not arrays")),
        Err(error) => {
            let message = error.to_string();
            if message.contains("recursion") || message.contains("surrogate") {
                Err(MethodError::Unsupported("JSON serde reads differently"))
            } else {
                Err(conduit_error(ERR_INVALID_DATA))
            }
        }
    }
}

/// The diff ID: an integer or its digits.
fn diff_id(value: Option<&Value>) -> Result<i64, MethodError> {
    match value {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text))
            if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            text.parse().ok()
        }
        _ => None,
    }
    .filter(|id| *id >= 0)
    .ok_or(MethodError::Unsupported("diff IDs which are not integers"))
}

/// `differential.setdiffproperty`.
pub struct SetDiffProperty;

impl SetDiffProperty {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let actor = diff_editor::actor_phid(request)?;
        let name = match request.value("name") {
            None | Some(Value::Null) => return Err(conduit_error(ERR_NO_NAME)),
            Some(Value::String(name)) if name.is_empty() => {
                return Err(conduit_error(ERR_NO_NAME));
            }
            Some(Value::String(name)) => name.clone(),
            Some(_) => return Err(MethodError::Unsupported("names which are not strings")),
        };
        let Some(Value::String(data)) = request.value("data") else {
            return Err(MethodError::Unsupported("data which is not a string"));
        };
        let data = decode(data)?;
        let encoded = diff_editor::encode(&data)?;
        if name.chars().count() > 128 {
            return Err(MethodError::Unsupported("values longer than their column"));
        }
        let id = diff_id(request.value("diff_id"))?;
        let diff = crate::diff_search::execute_one(request, id)
            .await?
            .ok_or_else(|| conduit_error(ERR_NOT_FOUND))?;
        let database = &request.context.database;
        if let Some(revision_id) = diff.revision_id.filter(|id| *id != 0) {
            let revisions = crate::storage::load_revisions_by_id(database, &[revision_id]).await?;
            let Some(revision) = revisions.into_iter().next() else {
                return Err(conduit_error(ERR_PERMISSIONS));
            };
            let config = &request.context.config;
            let visible = crate::storage::visible_revisions(
                database,
                config,
                &request.viewer,
                vec![revision.clone()],
                request.now,
            )
            .await
            .map_err(MethodError::internal)?;
            let editable =
                crate::storage::can_edit(database, config, &request.viewer, &revision, request.now)
                    .await
                    .map_err(MethodError::internal)?;
            if visible.is_empty() || !editable {
                return Err(MethodError::Unsupported(
                    "revisions the viewer can't edit, a policy exception",
                ));
            }
        } else if diff.author_phid.as_deref() != Some(actor.as_str()) {
            return Err(conduit_error(ERR_PERMISSIONS));
        }
        let table = database.table("differential", "differential_diffproperty");
        let sql = format!("SELECT id FROM {table} WHERE diffID = ? AND name = ?");
        let existing = phab_db::sqlx::query(&sql)
            .bind(id)
            .bind(&name)
            .fetch_optional(database.pool())
            .await?;
        match existing {
            Some(row) => {
                phab_db::sqlx::query(&format!(
                    "UPDATE {table} SET data = ?, dateModified = ? WHERE id = ?"
                ))
                .bind(&encoded)
                .bind(request.now)
                .bind(rows::int(&row, "id")?)
                .execute(database.pool())
                .await?;
            }
            None => {
                phab_db::sqlx::query(&format!(
                    "INSERT INTO {table} (diffID, name, data, dateCreated, dateModified) \
                     VALUES (?, ?, ?, ?, ?)"
                ))
                .bind(id)
                .bind(&name)
                .bind(&encoded)
                .bind(request.now)
                .bind(request.now)
                .execute(database.pool())
                .await?;
            }
        }
        Ok(Value::Null)
    }
}

impl Method for SetDiffProperty {
    fn name(&self) -> &'static str {
        "differential.setdiffproperty"
    }

    fn params(&self) -> &'static [&'static str] {
        &["diff_id", "name", "data"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[
            (ERR_NOT_FOUND, "Diff was not found."),
            (
                ERR_PERMISSIONS,
                "You do not have permission to modify this diff.",
            ),
            (ERR_INVALID_DATA, "Property data is not valid JSON."),
            (ERR_NO_NAME, "Name cannot be empty."),
        ]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn decodes_arrays_only() {
        assert_eq!(decode(r#"{"a": [1]}"#).unwrap(), json!({"a": [1]}));
        assert!(matches!(
            decode("nope"),
            Err(MethodError::Conduit { code, .. }) if code == ERR_INVALID_DATA
        ));
        assert!(matches!(decode("5"), Err(MethodError::Unsupported(_))));
    }
}
