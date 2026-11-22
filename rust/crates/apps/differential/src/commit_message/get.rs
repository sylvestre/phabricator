//! `differential.getcommitmessage`
//! (`DifferentialGetCommitMessageConduitAPIMethod`): a revision as a
//! commit message, to read or to edit.
//!
//! Ported: existing revisions in the "read", "edit" and "create" modes,
//! with `fields` overriding values. Left to PHP: the template of a new
//! revision (no `revision_id`), values of unusual types and lists naming
//! objects other than users and projects.

use phab_conduit::pager::{self, Direction};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::edges;
use serde_json::Value;

use super::fields::{self, DEFAULT_TITLE, Field, Kind};
use super::objects;
use crate::search::{self, Constraints, Order, Revisions};
use crate::storage::Revision;

/// `differential.getcommitmessage`.
pub struct GetCommitMessage;

/// The `edit` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Read,
    Edit,
    Create,
}

/// A field value, as `readFieldValueFromConduit()` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FieldValue {
    Text(Option<String>),
    List(Vec<String>),
}

/// `ERR_NOT_FOUND`.
fn not_found() -> MethodError {
    MethodError::Conduit {
        code: "ERR_NOT_FOUND".to_owned(),
        info: None,
    }
}

/// `optional revision_id`: `None` when falsy.
fn revision_id(value: Option<&Value>) -> Result<Option<i64>, MethodError> {
    let unusual = || MethodError::Unsupported("an unusual revision ID");
    match value {
        None | Some(Value::Null | Value::Bool(false)) => Ok(None),
        Some(Value::Number(number)) if !number.is_f64() => match number.as_i64() {
            Some(0) => Ok(None),
            Some(id) if id > 0 => Ok(Some(id)),
            _ => Err(unusual()),
        },
        Some(Value::String(text)) if text.is_empty() || text == "0" => Ok(None),
        Some(Value::String(text))
            if text.bytes().all(|byte| byte.is_ascii_digit()) && !text.starts_with('0') =>
        {
            text.parse().map(Some).map_err(|_| unusual())
        }
        Some(_) => Err(unusual()),
    }
}

fn mode(value: Option<&Value>) -> Result<Mode, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(Mode::Read),
        Some(Value::String(text)) if text.is_empty() => Ok(Mode::Read),
        Some(Value::String(text)) if text == "edit" => Ok(Mode::Edit),
        Some(Value::String(text)) if text == "create" => Ok(Mode::Create),
        Some(_) => Err(MethodError::Unsupported("an unusual edit mode")),
    }
}

/// `executeOne()` of the revision with its ID.
async fn load_revision(request: &ApiRequest<'_>, id: i64) -> Result<Revision, MethodError> {
    let query = Revisions {
        request,
        constraints: Constraints {
            ids: Some(vec![id]),
            ..Constraints::default()
        },
        order: Order::Updated,
        memory: std::sync::Mutex::default(),
    };
    if let Some(revision) = pager::execute(&query, None, Direction::Forward, 1)
        .await?
        .into_iter()
        .next()
    {
        return Ok(revision);
    }
    // executeOne() raises a policy exception for a revision the viewer
    // can't see.
    let database = &request.context.database;
    let sql = format!(
        "SELECT 1 FROM {} WHERE id = ?",
        database.table("differential", "differential_revision")
    );
    if phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(MethodError::Unsupported("a revision the viewer can't see"));
    }
    Err(not_found())
}

/// What the fields read from the revision (`readFieldValueFromObject()`).
struct Stored {
    summary: String,
    test_plan: String,
    /// `(reviewer, status)`, in the order of `loadReviewers()`.
    reviewers: Vec<(String, String)>,
    subscribers: Vec<String>,
    projects: Vec<String>,
    /// `loadCustomFieldStorage()`, by custom field key.
    custom: Vec<(&'static str, String)>,
}

impl Stored {
    async fn load(request: &ApiRequest<'_>, revision: &Revision) -> Result<Self, MethodError> {
        let database = &request.context.database;
        let revisions = std::slice::from_ref(revision);
        let extra = search::load_extra(database, revisions)
            .await?
            .remove(&revision.id)
            .ok_or_else(|| MethodError::internal("revision row vanished"))?;
        let reviewers = search::load_reviewers(database, revisions)
            .await?
            .remove(&revision.phid)
            .unwrap_or_default()
            .into_iter()
            .map(|row| (row.reviewer, row.status))
            .collect();
        let list = |phids: Option<Vec<Phid>>| -> Vec<String> {
            phids
                .unwrap_or_default()
                .into_iter()
                .map(Phid::into_string)
                .collect()
        };
        let subscribers = list(
            search::load_edge_lists(database, revisions, edges::OBJECT_HAS_SUBSCRIBER)
                .await?
                .remove(&revision.phid),
        );
        // Oldest first: the edge query is newest first and the field
        // reverses it.
        let mut projects = list(
            search::load_edge_lists(database, revisions, edges::OBJECT_HAS_PROJECT)
                .await?
                .remove(&revision.phid),
        );
        projects.reverse();
        Ok(Self {
            summary: extra.summary,
            test_plan: extra.test_plan,
            reviewers,
            subscribers,
            projects,
            custom: load_custom_storage(request, revision).await?,
        })
    }

    fn custom(&self, key: &str) -> Option<&str> {
        self.custom
            .iter()
            .find(|(known, _)| *known == key)
            .map(|(_, value)| value.as_str())
    }
}

/// The custom field storage the commit message fields read.
async fn load_custom_storage(
    request: &ApiRequest<'_>,
    revision: &Revision,
) -> Result<Vec<(&'static str, String)>, MethodError> {
    let keys = [
        "phabricator:auditors",
        "differential:bugzilla-bug-id",
        "phabricator:blame-revision",
        "phabricator:revert-plan",
    ];
    let indexes: Vec<(String, &'static str)> = keys
        .into_iter()
        .map(|key| (phab_core::hash::digest_for_index(key.as_bytes()), key))
        .collect();
    let database = &request.context.database;
    let sql = format!(
        "SELECT fieldIndex, fieldValue FROM {} WHERE objectPHID = ? AND fieldIndex IN ({})",
        database.table("differential", "differential_customfieldstorage"),
        phab_db::placeholders(indexes.len())
    );
    let query = phab_db::sqlx::query(&sql).bind(revision.phid.as_str());
    let rows = phab_db::bind_all(query, indexes.iter().map(|(index, _)| index.as_str()))
        .fetch_all(database.pool())
        .await?;
    let mut values = Vec::new();
    for row in &rows {
        let index = phab_db::rows::text(row, "fieldIndex")?;
        if let Some((_, key)) = indexes.iter().find(|(known, _)| *known == index) {
            values.push((*key, phab_db::rows::text(row, "fieldValue")?));
        }
    }
    Ok(values)
}

/// `readFieldValueFromObject()`, then `readFieldValueFromConduit()`.
fn object_value(
    field: Field,
    revision: &Revision,
    stored: &Stored,
) -> Result<FieldValue, MethodError> {
    let text = |value: &str| FieldValue::Text(Some(value.to_owned()));
    Ok(match field {
        Field::Title if revision.title.is_empty() => text(DEFAULT_TITLE),
        Field::Title => text(&revision.title),
        Field::Summary => text(&stored.summary),
        Field::TestPlan => text(&stored.test_plan),
        Field::Reviewers => FieldValue::List(
            stored
                .reviewers
                .iter()
                .map(|(phid, status)| match status.as_str() {
                    "blocking" => format!("{phid}!"),
                    _ => phid.clone(),
                })
                .collect(),
        ),
        Field::ReviewedBy => FieldValue::List(
            stored
                .reviewers
                .iter()
                .filter(|(_, status)| status == "accepted")
                .map(|(phid, _)| phid.clone())
                .collect(),
        ),
        Field::Subscribers => FieldValue::List(stored.subscribers.clone()),
        Field::Tags => FieldValue::List(stored.projects.clone()),
        // DifferentialAuditorsField stores JSON; no row is an empty list.
        Field::Auditors => match stored.custom("phabricator:auditors") {
            None => FieldValue::List(Vec::new()),
            Some(json) => match serde_json::from_str::<Value>(json) {
                Ok(Value::Array(items)) => FieldValue::List(
                    items
                        .into_iter()
                        .map(|item| match item {
                            Value::String(phid) => Ok(phid),
                            _ => Err(MethodError::Unsupported("unusual auditors")),
                        })
                        .collect::<Result<_, _>>()?,
                ),
                _ => return Err(MethodError::Unsupported("unusual auditors")),
            },
        },
        Field::BugId | Field::BlameRevision | Field::RevertPlan => FieldValue::Text(
            field
                .custom_key()
                .and_then(|key| stored.custom(key))
                .map(str::to_owned),
        ),
        Field::RevisionId => FieldValue::Text(Some(revision.id.to_string())),
        Field::Conflicts | Field::GitSvnId => FieldValue::Text(None),
    })
}

/// `readFieldValueFromConduit()` of a value of `fields`; values PHP
/// refuses with an exception are left to it.
fn wire_value(field: Field, value: &Value) -> Result<FieldValue, MethodError> {
    let unusual = || MethodError::Unsupported("commit message field values of unusual types");
    match (field.kind(), value) {
        (Kind::Text | Kind::RevisionId, Value::Null) => Ok(FieldValue::Text(None)),
        (Kind::Text | Kind::RevisionId, Value::String(text)) => {
            Ok(FieldValue::Text(Some(text.clone())))
        }
        (Kind::RevisionId, Value::Number(number)) if !number.is_f64() => {
            Ok(FieldValue::Text(Some(number.to_string())))
        }
        (Kind::List, Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(unusual))
            .collect::<Result<_, _>>()
            .map(FieldValue::List),
        _ => Err(unusual()),
    }
}

/// `PhabricatorEnv::getProductionURI()` of a path.
pub(super) fn production_uri(request: &ApiRequest<'_>, path: &str) -> Result<String, MethodError> {
    let config = &request.context.config;
    let base = match config.nonempty_string("phabricator.production-uri") {
        Some(uri) => uri,
        None => config.base_uri().map_err(MethodError::internal)?,
    };
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

/// `renderFieldValue()`.
async fn render(
    request: &ApiRequest<'_>,
    field: Field,
    value: &FieldValue,
) -> Result<Option<String>, MethodError> {
    match value {
        FieldValue::Text(None) => Ok(None),
        FieldValue::Text(Some(text)) if text.is_empty() => Ok(None),
        FieldValue::Text(Some(text)) if field == Field::RevisionId => {
            production_uri(request, &format!("/D{text}")).map(Some)
        }
        FieldValue::Text(Some(text)) => Ok(Some(text.clone())),
        FieldValue::List(list) if list.is_empty() => Ok(None),
        FieldValue::List(list) => {
            // inflateReviewers(): a trailing "!" is a blocking reviewer.
            let mut phids: Vec<String> = Vec::new();
            let mut blocking: Vec<String> = Vec::new();
            for item in list {
                match item.strip_suffix('!').filter(|_| field == Field::Reviewers) {
                    Some(phid) => {
                        phids.push(phid.to_owned());
                        blocking.push(phid.to_owned());
                    }
                    None => phids.push(item.clone()),
                }
            }
            let names = objects::command_line_names(request, &phids).await?;
            let mut unique: Vec<&String> = Vec::new();
            for phid in &phids {
                if !unique.contains(&phid) {
                    unique.push(phid);
                }
            }
            let tokens: Vec<String> = unique
                .iter()
                .zip(names)
                .map(|(phid, name)| {
                    if blocking.contains(phid) {
                        format!("{name}!")
                    } else {
                        name
                    }
                })
                .collect();
            Ok(Some(tokens.join(", ")))
        }
    }
}

impl Method for GetCommitMessage {
    fn name(&self) -> &'static str {
        "differential.getcommitmessage"
    }

    fn params(&self) -> &'static [&'static str] {
        &["revision_id", "fields", "edit"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(search::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[("ERR_NOT_FOUND", "Revision was not found.")]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let id = revision_id(request.value("revision_id"))?
                .ok_or(MethodError::Unsupported("the template of a new revision"))?;
            let revision = load_revision(request, id).await?;
            let mode = mode(request.value("edit"))?;
            let empty = serde_json::Map::new();
            let overrides = match request.value("fields") {
                Some(Value::Object(map)) => map,
                // A list has integer keys, which no field has.
                None | Some(Value::Array(_)) => &empty,
                Some(_) => return Err(MethodError::Unsupported("unusual commit message fields")),
            };

            let mut fields = fields::enabled_fields(&request.context.config)?;
            if mode != Mode::Read {
                fields.retain(|field| field.is_editable());
            }
            let stored = Stored::load(request, &revision).await?;
            let mut message: Vec<String> = Vec::new();
            for field in fields {
                let value = match overrides.get(field.key()) {
                    Some(value) => wire_value(field, value)?,
                    None => object_value(field, &revision, &stored)?,
                };
                let label = field.name();
                match render(request, field, &value).await? {
                    None if mode == Mode::Create && field.is_template() => {
                        message.push(format!("{label}: "));
                    }
                    None => {}
                    Some(value) if field == Field::Title => message.push(value),
                    Some(value) => {
                        let value = value.replace("\r\n", "\n").replace('\r', "\n");
                        if value.contains('\n') || value.starts_with("  ") {
                            message.push(format!("{label}:\n{value}"));
                        } else {
                            message.push(format!("{label}: {value}"));
                        }
                    }
                }
            }
            Ok(Value::String(message.join("\n\n")))
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_revision_ids_like_php() {
        assert_eq!(revision_id(None).unwrap(), None);
        assert_eq!(revision_id(Some(&json!(0))).unwrap(), None);
        assert_eq!(revision_id(Some(&json!(""))).unwrap(), None);
        assert_eq!(revision_id(Some(&json!(12))).unwrap(), Some(12));
        assert_eq!(revision_id(Some(&json!("12"))).unwrap(), Some(12));
        assert!(revision_id(Some(&json!("012"))).is_err());
        assert!(revision_id(Some(&json!("12a"))).is_err());
        assert!(revision_id(Some(&json!(1.5))).is_err());
    }

    #[test]
    fn reads_wire_values_like_php() {
        assert_eq!(
            wire_value(Field::RevisionId, &json!(3)).unwrap(),
            FieldValue::Text(Some("3".to_owned()))
        );
        assert_eq!(
            wire_value(Field::Summary, &json!(null)).unwrap(),
            FieldValue::Text(None)
        );
        assert!(wire_value(Field::Summary, &json!(3)).is_err());
        assert!(wire_value(Field::Reviewers, &json!(null)).is_err());
        assert!(wire_value(Field::Reviewers, &json!([1])).is_err());
    }
}
