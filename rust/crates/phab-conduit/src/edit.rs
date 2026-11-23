//! What the `*.edit` methods of `PhabricatorEditEngine`
//! (`buildConduitResponse()`) share, and the rows
//! `PhabricatorApplicationTransactionEditor` writes for the editors which
//! send no mail, publish no feed story and apply no Herald rules: the
//! transactions, then a `PhabricatorApplicationTransactionPublishWorker`
//! task, which indexes the object when the editor supports search.
//!
//! The methods built on it edit existing objects with transaction types
//! whose values are plain; anything else is [`MethodError::Unsupported`]
//! before anything is written.

use phab_core::{Phid, PhidType, php, random};
use phab_db::editor::{self as db_editor, CONDUIT_SOURCE, TransactionRow};
use phab_db::sqlx::MySqlConnection;
use phab_db::{Database, rows, worker};
use serde_json::{Map, Value, json};

use crate::{ApiRequest, MethodError, pager};

/// One transaction of the `transactions` parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct RawTransaction {
    /// `type`: the edit type.
    pub kind: String,
    /// `value`.
    pub value: Value,
}

/// `getRawConduitTransactions()`: a list of dictionaries with a `type`
/// and a `value`. PHP's exceptions for anything else quote the keys, and
/// are left to PHP; so are types which are not strings.
pub fn raw_transactions(request: &ApiRequest<'_>) -> Result<Vec<RawTransaction>, MethodError> {
    let items: Vec<&Value> = match request.value("transactions") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(items)) if items.is_empty() => Vec::new(),
        _ => return Err(MethodError::Unsupported("unusual transaction lists")),
    };
    items
        .into_iter()
        .map(|item| match (item.get("type"), item.get("value")) {
            (Some(Value::String(kind)), Some(value)) if item.is_object() => Ok(RawTransaction {
                kind: kind.clone(),
                value: value.clone(),
            }),
            _ => Err(MethodError::Unsupported("unusual transactions")),
        })
        .collect()
}

/// How `newObjectFromIdentifier()` finds the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identifier {
    /// An ID, given as an integer or a string of digits.
    Id(i64),
    /// A PHID.
    Phid(String),
}

/// The `objectIdentifier` of an edit, `None` when it is falsy (a new
/// object). Monograms and other names are left to PHP.
pub fn identifier(request: &ApiRequest<'_>) -> Result<Option<Identifier>, MethodError> {
    let Some(value) = request
        .value("objectIdentifier")
        .filter(|value| php::is_truthy(value))
    else {
        return Ok(None);
    };
    let identifier = match value {
        Value::Number(number) => number.as_i64().map(Identifier::Id),
        Value::String(text) if text.bytes().all(|byte| byte.is_ascii_digit()) => {
            text.parse().ok().map(Identifier::Id)
        }
        Value::String(text) if text.starts_with("PHID-") => Some(Identifier::Phid(text.clone())),
        _ => None,
    };
    identifier.map(Some).ok_or(MethodError::Unsupported(
        "identifiers other than IDs and PHIDs",
    ))
}

/// Edit forms configured in the web UI may change the fields of a
/// configurable engine.
pub async fn check_configuration(
    request: &ApiRequest<'_>,
    engine_key: &str,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT id FROM {} WHERE engineKey = ? LIMIT 1",
        database.table("search", "search_editengineconfiguration")
    );
    if phab_db::sqlx::query(&sql)
        .bind(engine_key)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(MethodError::Unsupported("configured edit forms"));
    }
    Ok(())
}

/// Refuses to write in shadow calls and on read-only installs, which PHP
/// answers alone.
pub fn require_writes(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    if request.writes == phab_auth::Writes::Allowed {
        Ok(())
    } else {
        Err(MethodError::Unsupported("writes when they are skipped"))
    }
}

/// The acting user: the omnipotent user's edits are left to PHP.
pub fn actor(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    request
        .viewer
        .phid
        .as_ref()
        .map(|phid| phid.as_str().to_owned())
        .ok_or(MethodError::Unsupported("edits by the omnipotent user"))
}

/// The exception of a value its parameter type refuses
/// (`raiseValidationException()`, wrapped by `getConduitTransactions()`).
fn refused(kind: &str, message: &str) -> MethodError {
    pager::core_error(format!(
        "Exception when processing transaction of type \"{kind}\": Error while reading \
         \"value\": {message}"
    ))
}

/// The value of a `string` edit type (`ConduitStringParameterType`) of a
/// transaction of type `kind`.
pub fn string(kind: &str, value: &Value) -> Result<String, MethodError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| refused(kind, "Expected string, got something else."))
}

/// The value of a `bool` edit type (`ConduitBoolParameterType`): booleans,
/// and in form-encoded calls `0`, `1`, `false` and `true` in any case.
pub fn boolean(kind: &str, value: &Value, strictly_typed: bool) -> Result<bool, MethodError> {
    match value {
        Value::Bool(flag) => return Ok(*flag),
        Value::String(text) if !strictly_typed => {
            if !text.is_ascii() {
                return Err(MethodError::Unsupported(
                    "booleans which are not ASCII strings",
                ));
            }
            match text.to_ascii_lowercase().as_str() {
                "0" | "false" => return Ok(false),
                "1" | "true" => return Ok(true),
                _ => {}
            }
        }
        _ => {}
    }
    Err(refused(
        kind,
        "Expected boolean (true or false), got something else.",
    ))
}

/// The value of a `list<string>` edit type (`ConduitStringListParameterType`).
pub fn string_list(value: &Value) -> Result<Vec<String>, MethodError> {
    let Value::Array(items) = value else {
        return Err(MethodError::Unsupported("lists which are not lists"));
    };
    items
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or(MethodError::Unsupported(
            "lists of values other than strings",
        ))
}

/// At most one transaction of each type: PHP merges or repeats them.
pub fn check_unique(kinds: &[&str]) -> Result<(), MethodError> {
    for (index, kind) in kinds.iter().enumerate() {
        if kinds[..index].contains(kind) {
            return Err(MethodError::Unsupported("several transactions of one type"));
        }
    }
    Ok(())
}

/// `PhabricatorApplicationTransactionValidationException` of these
/// errors, in order.
pub fn validation_error(errors: &[String]) -> MethodError {
    let mut message = "Validation errors:".to_owned();
    for error in errors {
        message.push_str("\n  - ");
        message.push_str(error);
    }
    pager::core_error(message)
}

/// `isEmptyTextTransaction()`: whether the field is empty once the
/// transactions apply (the last one wins).
pub fn is_empty_text(current: Option<&str>, new_values: &[&str]) -> bool {
    new_values
        .last()
        .copied()
        .or(current)
        .is_none_or(str::is_empty)
}

/// A transaction with an effect, with its values as the editor stores
/// them.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    /// The transaction PHID.
    pub phid: String,
    /// `transactionType`.
    pub kind: String,
    /// The old value.
    pub old: Value,
    /// The new value.
    pub new: Value,
    /// The metadata the edit type sets, before `core.groupID`.
    pub metadata: Map<String, Value>,
}

impl Planned {
    /// A transaction of an object of `object_type` without metadata.
    pub fn new(object_type: PhidType, kind: &str, (old, new): (Value, Value)) -> Self {
        Self {
            phid: Phid::generate(PhidType::TRANSACTION, Some(object_type)).into_string(),
            kind: kind.to_owned(),
            old,
            new,
            metadata: Map::new(),
        }
    }

    /// Whether it has an effect (`getTransactionHasEffect()`: `$old !==
    /// $new`).
    pub fn has_effect(&self) -> bool {
        self.old != self.new
    }
}

/// What an edit writes around the object's own rows.
#[derive(Debug, Clone)]
pub struct Publication {
    /// The acting user.
    pub actor: String,
    /// The edited object.
    pub object: String,
    /// The transactions, in order.
    pub transactions: Vec<Planned>,
    /// `getTransactionGroupID()`.
    pub group_id: String,
    /// `loadXHeraldRulesHeader()`.
    pub herald_header: Option<String>,
}

impl Publication {
    /// The publication of `transactions` on `object`, reading its saved
    /// Herald header.
    pub async fn new(
        database: &Database,
        actor: String,
        object: String,
        transactions: Vec<Planned>,
    ) -> Result<Self, MethodError> {
        let sql = format!(
            "SELECT header FROM {} WHERE phid = ?",
            database.table("herald", "herald_savedheader")
        );
        let herald_header = phab_db::sqlx::query(&sql)
            .bind(&object)
            .fetch_optional(database.pool())
            .await?
            .map(|row| rows::text(&row, "header"))
            .transpose()?;
        Ok(Self {
            actor,
            object,
            transactions,
            group_id: random::random_characters(32),
            herald_header,
        })
    }

    /// The transaction PHIDs.
    pub fn transaction_phids(&self) -> Vec<String> {
        self.transactions
            .iter()
            .map(|planned| planned.phid.clone())
            .collect()
    }

    /// Writes the transactions in `table` (like `database.table("dashboard",
    /// "dashboard_paneltransaction")`).
    pub async fn write_transactions(
        &self,
        connection: &mut MySqlConnection,
        table: &str,
        now: i64,
    ) -> Result<(), MethodError> {
        for planned in &self.transactions {
            let mut metadata = planned.metadata.clone();
            metadata.insert("core.groupID".to_owned(), json!(self.group_id));
            let (old, new, metadata) = (
                php::json_encode(&planned.old),
                php::json_encode(&planned.new),
                php::json_encode(&Value::Object(metadata)),
            );
            let row = TransactionRow {
                phid: &planned.phid,
                author: &self.actor,
                object: &self.object,
                kind: &planned.kind,
                old: &old,
                new: &new,
                metadata: &metadata,
                content_source: CONDUIT_SOURCE,
            };
            db_editor::insert_transaction(connection, table, row, now).await?;
        }
        Ok(())
    }

    /// The `PhabricatorApplicationTransactionPublishWorker` task of an
    /// editor which sends no mail and publishes no feed story.
    pub async fn schedule(
        &self,
        connection: &mut MySqlConnection,
        database: &Database,
        now: i64,
    ) -> Result<(), MethodError> {
        let data = php::json_encode(&json!({
            "objectPHID": self.object,
            "actorPHID": self.actor,
            "xactionPHIDs": self.transaction_phids(),
            "state": {
                "parentMessageID": null,
                "isNewObject": false,
                "heraldEmailPHIDs": [],
                "heraldForcedEmailPHIDs": [],
                "heraldHeader": self.herald_header,
                "mailToPHIDs": [],
                "mailCCPHIDs": [],
                "feedNotifyPHIDs": [],
                "feedRelatedPHIDs": [],
                "feedShouldPublish": false,
                "mailShouldSend": false,
                "mustEncrypt": [],
                "mailStamps": [],
                "mailUnexpandablePHIDs": [],
                "mailMutedPHIDs": [],
                "webhookMap": [],
                "silent": null,
                "sendHistory": false,
                "excludeMailRecipientPHIDs": [],
                "custom": [],
                "custom.encoding": [],
            },
        }));
        let task = worker::Task {
            class: "PhabricatorApplicationTransactionPublishWorker",
            data: &data,
            priority: worker::PRIORITY_ALERTS,
            object_phid: Some(&self.object),
        };
        worker::schedule_task(connection, database, task, now).await?;
        Ok(())
    }

    /// `buildConduitResponse()`'s answer.
    pub fn response(&self, id: i64) -> Value {
        json!({
            "object": {"id": id, "phid": self.object},
            "transactions": self
                .transactions
                .iter()
                .map(|planned| json!({"phid": planned.phid}))
                .collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_empty_text_like_php() {
        assert!(is_empty_text(None, &[]));
        assert!(is_empty_text(Some(""), &[]));
        assert!(!is_empty_text(Some("x"), &[]));
        assert!(is_empty_text(Some("x"), &["y", ""]));
        assert!(!is_empty_text(Some(""), &["y"]));
    }

    #[test]
    fn lists_validation_errors() {
        assert_eq!(
            validation_error(&["A.".to_owned(), "B.".to_owned()]),
            pager::core_error("Validation errors:\n  - A.\n  - B.".to_owned())
        );
    }

    #[test]
    fn finds_repeated_types() {
        assert!(check_unique(&["name", "text"]).is_ok());
        assert!(check_unique(&["name", "text", "name"]).is_err());
    }
}
