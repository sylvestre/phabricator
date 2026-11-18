//! `differential.querydiffs` (`DifferentialQueryDiffsConduitAPIMethod`):
//! the diffs with these IDs, or of these revisions, with their changes,
//! hunks and properties (`DifferentialDiff::getDiffDict()`).

use phab_conduit::pager::Direction;
use phab_conduit::search::IdOrder;
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::php;
use phab_db::{Database, rows};
use phab_diff::Changeset;
use phab_diff::storage::{self as diff_storage, StorageError};
use serde_json::{Map, Value, json};

use crate::diff_search::{self, DiffRow};

/// A `list<uint>` parameter: absent, `null` or empty is no constraint.
fn id_list(value: Option<&Value>) -> Result<Vec<i64>, MethodError> {
    let list = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(list)) => list,
        Some(_) => return Err(MethodError::Unsupported("ID lists which are not lists")),
    };
    list.iter()
        .map(|id| match id {
            Value::Number(number) if !number.is_f64() => number.as_i64(),
            Value::String(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                text.parse().ok()
            }
            _ => None,
        })
        .collect::<Option<_>>()
        .ok_or(MethodError::Unsupported("IDs which are not integers"))
}

fn storage_error(error: StorageError) -> MethodError {
    match error {
        StorageError::Database(error) => MethodError::internal(error),
        _ => MethodError::Unsupported("hunks Rust can't read"),
    }
}

/// The columns of `getDiffDict()` beyond [`DiffRow`].
async fn diff_columns(database: &Database, id: i64) -> Result<Map<String, Value>, MethodError> {
    let sql = format!(
        "SELECT revisionID, dateCreated, dateModified, sourceControlBaseRevision, \
           sourceControlPath, sourceControlSystem, branch, bookmark, creationMethod, \
           description, unitStatus, lintStatus FROM {} WHERE id = ?",
        database.table("differential", "differential_diff")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_one(database.pool())
        .await?;
    let text = |column: &str| -> Result<Value, MethodError> {
        Ok(rows::opt_text(&row, column)?.map_or(Value::Null, Value::String))
    };
    // Lisk objects loaded without a type cast keep MySQL's strings.
    let int = |column: &str| -> Result<Value, MethodError> {
        Ok(rows::opt_int(&row, column)?.map_or(Value::Null, |value| json!(value.to_string())))
    };
    let mut dict = Map::new();
    dict.insert("id".to_owned(), json!(id.to_string()));
    dict.insert("revisionID".to_owned(), int("revisionID")?);
    dict.insert("dateCreated".to_owned(), int("dateCreated")?);
    dict.insert("dateModified".to_owned(), int("dateModified")?);
    for column in [
        "sourceControlBaseRevision",
        "sourceControlPath",
        "sourceControlSystem",
        "branch",
        "bookmark",
        "creationMethod",
        "description",
    ] {
        dict.insert(column.to_owned(), text(column)?);
    }
    dict.insert("unitStatus".to_owned(), int("unitStatus")?);
    dict.insert("lintStatus".to_owned(), int("lintStatus")?);
    Ok(dict)
}

/// `buildChangesList()`. The integers are MySQL's strings, as PHP keeps
/// them.
fn changes(changesets: &[Changeset]) -> Vec<Value> {
    changesets
        .iter()
        .map(|changeset| {
            let hunks: Vec<Value> = changeset
                .hunks
                .iter()
                .map(|hunk| {
                    json!({
                        "oldOffset": hunk.old_offset.to_string(),
                        "newOffset": hunk.new_offset.to_string(),
                        "oldLength": hunk.old_len.to_string(),
                        "newLength": hunk.new_len.to_string(),
                        "addLines": null,
                        "delLines": null,
                        "isMissingOldNewline": null,
                        "isMissingNewNewline": null,
                        "corpus": hunk.changes,
                    })
                })
                .collect();
            json!({
                "id": changeset.id.to_string(),
                "metadata": php::round_trip(&changeset.metadata),
                "oldPath": changeset.old_file,
                "currentPath": changeset.filename,
                "awayPaths": changeset.away_paths,
                "oldProperties": php::round_trip(&changeset.old_properties),
                "newProperties": php::round_trip(&changeset.new_properties),
                "type": changeset.change_type.value().to_string(),
                "fileType": changeset.file_type.value().to_string(),
                "commitHash": null,
                "addLines": changeset.add_lines.to_string(),
                "delLines": changeset.del_lines.to_string(),
                "hunks": hunks,
            })
        })
        .collect()
}

/// `getDiffAuthorshipDict()`: the properties, and the author of the first
/// local commit.
async fn authorship(
    database: &Database,
    id: i64,
    dict: &mut Map<String, Value>,
) -> Result<(), MethodError> {
    let sql = format!(
        "SELECT name, data FROM {} WHERE diffID = ? ORDER BY id",
        database.table("differential", "differential_diffproperty")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_all(database.pool())
        .await?;
    let mut properties = Map::new();
    let mut author = None;
    for row in &found {
        let name = rows::text(row, "name")?;
        let data = rows::json(row, "data")?;
        if name == "local:commits" {
            let first = match &data {
                Value::Object(commits) => commits.values().next(),
                Value::Array(commits) => commits.first(),
                _ => return Err(MethodError::Unsupported("unusual local commits")),
            };
            if let Some(commit) = first {
                let author_name = commit
                    .get("author")
                    .ok_or(MethodError::Unsupported("local commits without an author"))?;
                author = Some((
                    author_name.clone(),
                    commit.get("authorEmail").cloned().unwrap_or(Value::Null),
                ));
            }
        }
        properties.insert(name, php::round_trip(&data));
    }
    let properties = if properties.is_empty() {
        Value::Array(Vec::new())
    } else {
        Value::Object(properties)
    };
    dict.insert("properties".to_owned(), properties);
    if let Some((name, email)) = author {
        dict.insert("authorName".to_owned(), name);
        dict.insert("authorEmail".to_owned(), email);
    }
    Ok(())
}

/// `getDiffDict()`.
pub(crate) async fn diff_dict(database: &Database, diff: &DiffRow) -> Result<Value, MethodError> {
    let mut dict = diff_columns(database, diff.id).await?;
    let changesets = diff_storage::load_changesets(database, diff.id)
        .await
        .map_err(storage_error)?;
    dict.insert("changes".to_owned(), Value::Array(changes(&changesets)));
    authorship(database, diff.id, &mut dict).await?;
    Ok(Value::Object(dict))
}

/// `differential.querydiffs`.
pub struct QueryDiffs;

impl Method for QueryDiffs {
    fn name(&self) -> &'static str {
        "differential.querydiffs"
    }

    fn params(&self) -> &'static [&'static str] {
        &["ids", "revisionIDs"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(crate::search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let ids = id_list(request.value("ids"))?;
            let revision_ids = id_list(request.value("revisionIDs"))?;
            if ids.is_empty() && revision_ids.is_empty() {
                // Pagination hadn't been invented yet in 2008.
                return Ok(json!([]));
            }
            let diffs = diff_search::query_diffs(
                request,
                (!ids.is_empty()).then_some(ids),
                (!revision_ids.is_empty()).then_some(revision_ids),
                IdOrder::Newest,
                Direction::Forward,
            )
            .await?;
            let database = &request.context.database;
            let mut result = Map::new();
            for diff in &diffs {
                result.insert(diff.id.to_string(), diff_dict(database, diff).await?);
            }
            // mpull() of nothing is an empty array.
            Ok(if result.is_empty() {
                json!([])
            } else {
                Value::Object(result)
            })
        })
    }
}

/// `differential.getalldiffs` (`DifferentialGetAllDiffsConduitAPIMethod`,
/// deprecated): the IDs of the diffs of revisions, newest first.
pub struct GetAllDiffs;

impl Method for GetAllDiffs {
    fn name(&self) -> &'static str {
        "differential.getalldiffs"
    }

    fn params(&self) -> &'static [&'static str] {
        &["revision_ids"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(crate::search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let revision_ids = match request.value("revision_ids") {
                Some(value) if php::is_truthy(value) => id_list(Some(value))?,
                _ => return Ok(json!([])),
            };
            let diffs = diff_search::query_diffs(
                request,
                None,
                Some(revision_ids),
                IdOrder::Newest,
                Direction::Forward,
            )
            .await?;
            // Lisk keeps MySQL's strings.
            Ok(Value::Array(
                diffs
                    .iter()
                    .map(|diff| {
                        json!({
                            "revision_id": diff.revision_id.map(|id| id.to_string()),
                            "diff_id": diff.id.to_string(),
                        })
                    })
                    .collect(),
            ))
        })
    }
}

/// An `optional id` parameter: `None` when falsy.
fn optional_id(value: Option<&Value>) -> Result<Option<i64>, MethodError> {
    match value {
        Some(value) if php::is_truthy(value) => match value {
            Value::Number(number) if !number.is_f64() => number.as_i64().map(Some),
            Value::String(text) if text.bytes().all(|b| b.is_ascii_digit()) => {
                text.parse().ok().map(Some)
            }
            _ => None,
        }
        .ok_or(MethodError::Unsupported("an unusual ID")),
        _ => Ok(None),
    }
}

/// `ERR_BAD_DIFF`.
fn bad_diff() -> MethodError {
    MethodError::Conduit {
        code: "ERR_BAD_DIFF".to_owned(),
        info: Some("No such diff exists.".to_owned()),
    }
}

/// `differential.getdiff` (`DifferentialGetDiffConduitAPIMethod`): a diff
/// by ID, or the latest visible diff of a revision, as `getDiffDict()`.
pub struct GetDiff;

impl Method for GetDiff {
    fn name(&self) -> &'static str {
        "differential.getdiff"
    }

    fn params(&self) -> &'static [&'static str] {
        &["revision_id", "diff_id"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(crate::search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let mut diff_id = optional_id(request.value("diff_id"))?;
            // A revision wins: its newest diff the viewer can see.
            if let Some(revision_id) = optional_id(request.value("revision_id"))? {
                let diffs = diff_search::query_diffs(
                    request,
                    None,
                    Some(vec![revision_id]),
                    IdOrder::Newest,
                    Direction::Forward,
                )
                .await?;
                diff_id = Some(diffs.first().ok_or_else(bad_diff)?.id);
            }
            let diff_id = diff_id.ok_or_else(bad_diff)?;
            let diff = diff_search::execute_one(request, diff_id)
                .await?
                .ok_or_else(bad_diff)?;
            diff_dict(&request.context.database, &diff).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_lists() {
        assert_eq!(id_list(None), Ok(Vec::new()));
        assert_eq!(id_list(Some(&json!([1, "2"]))), Ok(vec![1, 2]));
        assert!(id_list(Some(&json!(["x"]))).is_err());
        assert!(id_list(Some(&json!(3))).is_err());
    }
}
