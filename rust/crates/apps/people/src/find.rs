//! `user.find` (`UserFindConduitAPIMethod`): the PHIDs of users by
//! username, newest first.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_db::rows;
use serde_json::{Map, Value, json};

use crate::APPLICATION;

/// The `aliases` parameter; an empty list makes `qsprintf()` throw, which
/// is left to PHP.
fn aliases(value: Option<&Value>) -> Result<Vec<&str>, MethodError> {
    match value {
        Some(Value::Array(list)) if !list.is_empty() => list
            .iter()
            .map(Value::as_str)
            .collect::<Option<_>>()
            .ok_or(MethodError::Unsupported("aliases which are not strings")),
        _ => Err(MethodError::Unsupported("an empty or unusual alias list")),
    }
}

/// `user.find`.
pub struct UserFind;

impl Method for UserFind {
    fn name(&self) -> &'static str {
        "user.find"
    }

    fn params(&self) -> &'static [&'static str] {
        &["aliases"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let aliases = aliases(request.value("aliases"))?;
            let database = &request.context.database;
            let sql = format!(
                "SELECT phid, userName FROM {} WHERE userName IN ({}) ORDER BY id DESC",
                database.table("user", "user"),
                phab_db::placeholders(aliases.len())
            );
            let found = phab_db::bind_all(phab_db::sqlx::query(&sql), aliases.iter().copied())
                .fetch_all(database.pool())
                .await?;
            // mpull($users, 'getPHID', 'getUsername'); an empty result is a
            // PHP list.
            let mut result = Map::new();
            for row in &found {
                result.insert(
                    rows::text(row, "userName")?,
                    json!(rows::text(row, "phid")?),
                );
            }
            Ok(if result.is_empty() {
                json!([])
            } else {
                Value::Object(result)
            })
        })
    }
}
