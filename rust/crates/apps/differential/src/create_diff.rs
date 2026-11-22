//! `differential.creatediff` (`DifferentialCreateDiffConduitAPIMethod`):
//! a diff from arcanist's change dictionaries, as `arc` and `moz-phab`
//! send them.
//!
//! Ported: every parameter, for Git working copies. This fork's method has
//! no `properties` parameter (moz-phab sets them with
//! `differential.setdiffproperty`). Left to PHP: form-encoded calls, change dictionaries missing keys
//! or with values of types PHP would convert or reject, other version
//! control systems, a missing `repositoryPHID` when PHP would guess the
//! repository from the base commit or a repository UUID, and what
//! [`crate::diff_editor`] leaves to PHP.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::php;
use phab_diff::arcanist::{RawChange, RawHunk};
use serde_json::{Map, Value, json};

use crate::diff_editor::{self, DiffFields, PlannedDiff};

const ERR_NO_CONTENT: &str = "ERR-NO-CONTENT";
const ERR_NO_SOURCE_PATH: &str = "ERR-USAGE-NO-SOURCE-PATH";

fn conduit_error(code: &str) -> MethodError {
    MethodError::Conduit {
        code: code.to_owned(),
        info: None,
    }
}

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// A key of an arcanist dictionary, which PHP reads without checking it
/// exists (a missing one raises a warning, which is an exception).
fn key<'a>(dict: &'a Map<String, Value>, name: &str) -> Result<&'a Value, MethodError> {
    dict.get(name)
        .ok_or(unsupported("change dictionaries missing keys"))
}

/// The entries of a PHP array decoded from JSON, which `foreach` walks.
fn entries(value: &Value) -> Result<Vec<&Value>, MethodError> {
    match value {
        Value::Array(items) => Ok(items.iter().collect()),
        Value::Object(map) => Ok(map.values().collect()),
        _ => Err(unsupported("change dictionaries which are not arrays")),
    }
}

fn dictionary(value: &Value) -> Result<&Map<String, Value>, MethodError> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(unsupported("change dictionaries which are not maps")),
    }
}

/// An integer of a hunk or change, as the `uint32` column stores it.
fn integer(value: &Value) -> Result<i64, MethodError> {
    value
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .map(i64::from)
        .ok_or(unsupported("change values which are not 32-bit integers"))
}

/// `addLines` or `delLines`, which PHP adds to an integer.
fn line_count(value: &Value) -> Result<i64, MethodError> {
    match value {
        Value::Null => Ok(0),
        other => integer(other),
    }
}

/// A path: a string, or null.
fn path(value: &Value) -> Result<Option<String>, MethodError> {
    match value {
        Value::Null => Ok(None),
        Value::String(path) => Ok(Some(path.clone())),
        _ => Err(unsupported("paths which are not strings")),
    }
}

/// `nonempty($value, array())`.
fn nonempty(value: &Value) -> Value {
    if php::is_truthy(value) {
        value.clone()
    } else {
        Value::Array(Vec::new())
    }
}

/// `ArcanistDiffHunk::newFromDictionary()`.
fn hunk(value: &Value) -> Result<RawHunk, MethodError> {
    let dict = dictionary(value)?;
    for name in ["isMissingOldNewline", "isMissingNewNewline"] {
        key(dict, name)?;
    }
    let Value::String(corpus) = key(dict, "corpus")? else {
        return Err(unsupported("hunks whose corpus is not a string"));
    };
    Ok(RawHunk {
        old_offset: integer(key(dict, "oldOffset")?)?,
        old_length: integer(key(dict, "oldLength")?)?,
        new_offset: integer(key(dict, "newOffset")?)?,
        new_length: integer(key(dict, "newLength")?)?,
        add_lines: line_count(key(dict, "addLines")?)?,
        del_lines: line_count(key(dict, "delLines")?)?,
        corpus: corpus.clone(),
    })
}

/// `ArcanistDiffChange::newFromDictionary()`.
fn change(value: &Value) -> Result<RawChange, MethodError> {
    let dict = dictionary(value)?;
    let hunks = entries(key(dict, "hunks")?)?
        .into_iter()
        .map(hunk)
        .collect::<Result<Vec<_>, _>>()?;
    key(dict, "commitHash")?;
    let metadata = match key(dict, "metadata")? {
        Value::Null => None,
        Value::Object(map) => Some(map.clone()),
        Value::Array(list) => Some(
            list.iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value.clone()))
                .collect(),
        ),
        _ => return Err(unsupported("metadata which is not an array")),
    };
    Ok(RawChange {
        metadata,
        old_path: path(key(dict, "oldPath")?)?,
        current_path: path(key(dict, "currentPath")?)?,
        away_paths: nonempty(key(dict, "awayPaths")?),
        old_properties: nonempty(key(dict, "oldProperties")?),
        new_properties: nonempty(key(dict, "newProperties")?),
        change_type: integer(key(dict, "type")?)?,
        file_type: integer(key(dict, "fileType")?)?,
        hunks,
    })
}

/// `lintStatus` or `unitStatus`: `switch` compares loosely, so only
/// strings are ported.
fn status(request: &ApiRequest<'_>, key: &str) -> Result<i64, MethodError> {
    match request.value(key) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::String(status)) => Ok(match status.as_str() {
            "skip" => diff_editor::STATUS_SKIP,
            "okay" => 1,
            "warn" => 2,
            "fail" => 3,
            _ => 0,
        }),
        Some(_) => Err(unsupported("statuses which are not strings")),
    }
}

/// `sourcePath`, which PHP truncates to its column (left to PHP here).
fn source_path(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    match request.value("sourcePath") {
        None | Some(Value::Null) => Err(conduit_error(ERR_NO_SOURCE_PATH)),
        Some(Value::String(path)) if path.is_empty() => Err(conduit_error(ERR_NO_SOURCE_PATH)),
        Some(Value::String(path)) if path.len() <= 255 => Ok(path.clone()),
        Some(Value::String(_)) => Err(unsupported("source paths PHP truncates")),
        Some(_) => Err(unsupported("source paths which are not strings")),
    }
}

/// The repository, as the request gives it and the query finds it.
async fn repository(
    request: &ApiRequest<'_>,
) -> Result<(Option<String>, Option<String>), MethodError> {
    let uuid = diff_editor::text_value(request.value("repositoryUUID"))?;
    let phid = match request.value("repositoryPHID") {
        None | Some(Value::Null) => None,
        Some(Value::String(phid)) => Some(phid.clone()),
        Some(_) => return Err(unsupported("repository PHIDs which are not strings")),
    };
    if let Some(given) = phid.as_deref().filter(|phid| php::is_truthy_str(phid))
        && let Some((phid, uuid)) = diff_editor::repository(request, given).await?
    {
        return Ok((Some(phid), uuid));
    }
    Ok((phid, uuid))
}

/// The `TYPE_DIFF_CREATE` dictionary, after the steps of `execute()`
/// which precede it.
async fn fields(request: &ApiRequest<'_>, actor: String) -> Result<DiffFields, MethodError> {
    let (repository_phid, repository_uuid) = repository(request).await?;
    let lint_status = status(request, "lintStatus")?;
    let unit_status = status(request, "unitStatus")?;
    let source_path = source_path(request)?;
    let source_control_system = diff_editor::text_value(request.value("sourceControlSystem"))?;
    if source_control_system
        .as_deref()
        .is_some_and(|system| system != "git")
    {
        return Err(unsupported("version control systems other than Git"));
    }
    let text = |key| diff_editor::text_value(request.value(key));
    let fields = DiffFields {
        source_path: Some(source_path),
        source_machine: text("sourceMachine")?,
        branch: text("branch")?,
        creation_method: text("creationMethod")?,
        author_phid: actor,
        bookmark: text("bookmark")?,
        repository_phid,
        repository_uuid,
        source_control_system,
        source_control_path: text("sourceControlPath")?,
        source_control_base_revision: text("sourceControlBaseRevision")?,
        lint_status,
        unit_status,
    };
    let longer = |value: &Option<String>| value.as_ref().is_some_and(|value| value.len() > 64);
    if longer(&fields.repository_phid) || longer(&fields.repository_uuid) {
        return Err(unsupported("values longer than their column"));
    }
    // applyFinalEffects(): without a repository, PHP looks one up by its
    // UUID, then by the base commit.
    let truthy = |value: &Option<String>| value.as_deref().is_some_and(php::is_truthy_str);
    if !truthy(&fields.repository_phid)
        && (truthy(&fields.repository_uuid)
            || fields.source_control_system.is_some()
                && truthy(&fields.source_control_base_revision))
    {
        return Err(unsupported("diffs whose repository PHP must guess"));
    }
    Ok(fields)
}

/// `differential.creatediff`.
pub struct CreateDiff;

impl CreateDiff {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        if !request.strictly_typed {
            return Err(unsupported("form-encoded diffs"));
        }
        let actor = diff_editor::actor_phid(request)?;
        let change_data = request.value("changes").unwrap_or(&Value::Null);
        if !php::is_truthy(change_data) {
            return Err(conduit_error(ERR_NO_CONTENT));
        }
        let changes = entries(change_data)?
            .into_iter()
            .map(change)
            .collect::<Result<Vec<_>, _>>()?;
        let (changesets, line_count) = diff_editor::build_changesets(request, &changes).await?;
        let fields = fields(request, actor).await?;
        diff_editor::check_herald(request).await?;
        let planned = PlannedDiff {
            changesets,
            line_count,
            fields,
            properties: Vec::new(),
        };
        let (id, phid) = diff_editor::save(request, &planned).await?;
        let uri = request
            .context
            .config
            .uri(&format!("/differential/diff/{id}/"))?;
        Ok(json!({"diffid": id, "phid": phid, "uri": uri}))
    }
}

impl Method for CreateDiff {
    fn name(&self) -> &'static str {
        "differential.creatediff"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "changes",
            "sourceMachine",
            "sourcePath",
            "branch",
            "bookmark",
            "sourceControlSystem",
            "sourceControlPath",
            "sourceControlBaseRevision",
            "creationMethod",
            "lintStatus",
            "unitStatus",
            "repositoryPHID",
            "parentRevisionID",
            "authorPHID",
            "repositoryUUID",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::search::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[
            (ERR_NO_CONTENT, "Diff may not be empty."),
            (ERR_NO_SOURCE_PATH, "You must specify a source path."),
        ]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_changes_like_arcanist() {
        let change = change(&json!({
            "metadata": [],
            "oldPath": "a",
            "currentPath": "a",
            "awayPaths": [],
            "oldProperties": {},
            "newProperties": {"unix:filemode": "100644"},
            "type": 2,
            "fileType": 1,
            "commitHash": null,
            "hunks": [{
                "oldOffset": 1, "newOffset": 1, "oldLength": 1, "newLength": 1,
                "addLines": 1, "delLines": null,
                "isMissingOldNewline": false, "isMissingNewNewline": false,
                "corpus": "+x\n",
            }],
        }))
        .unwrap();
        assert_eq!(change.metadata, Some(Map::new()));
        assert_eq!(change.old_properties, json!([]));
        assert_eq!(change.new_properties, json!({"unix:filemode": "100644"}));
        assert_eq!(change.hunks[0].del_lines, 0);
        assert!(matches!(
            super::change(&json!({"hunks": []})),
            Err(MethodError::Unsupported(_))
        ));
    }
}
