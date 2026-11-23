//! The `core:customfield` transactions of the stored custom fields an edit
//! sets through Conduit (`PhabricatorCustomFieldEditType`): Mozilla's
//! Bugzilla bug ID (`DifferentialBugzillaBugIDField`, `bugzilla.bug-id`)
//! and uplift request (`DifferentialUpliftRequestCustomField`,
//! `uplift.request`), and the auditors (`DifferentialAuditorsField`,
//! `phabricator:auditors`).
//!
//! The new value is the field's storage value of the call's value
//! (`getTransactionValueFromValue()`), the old one the stored value read
//! back (`getOldValueForApplicationTransactions()`); an equal value has no
//! effect, and is dropped. The external effect writes the row of
//! `differential_customfieldstorage` (values are never `null` here).
//!
//! The bug ID is validated as `DifferentialBugzillaBugIDValidator` does,
//! even without effect: an empty one passes unless `bugzilla.require_bugs`,
//! a number passes for administrators; for everyone else PHP asks BMO,
//! which is left to it, as are the values PHP refuses (with an error) and
//! JSON values PHP may decode or encode back differently.

use phab_conduit::{ApiRequest, MethodError};
use phab_core::php;
use phab_db::rows;
use serde_json::{Value, json};

use super::Plan;
use crate::storage::Revision;

/// A stored custom field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    BugId,
    Uplift,
    Auditors,
}

impl Field {
    /// The field of a transaction type of the edit engine.
    pub(super) fn of_conduit_key(key: &str) -> Option<Self> {
        Some(match key {
            "bugzilla.bug-id" => Self::BugId,
            "uplift.request" => Self::Uplift,
            "phabricator:auditors" => Self::Auditors,
            _ => return None,
        })
    }

    /// `getFieldKey()`.
    fn key(self) -> &'static str {
        match self {
            Self::BugId => "differential:bugzilla-bug-id",
            Self::Uplift => "differential:uplift-request",
            Self::Auditors => "phabricator:auditors",
        }
    }
}

/// A field set by the call: the field and its storage value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Wanted {
    pub(super) field: Field,
    value: String,
}

/// The planned transaction.
pub(super) struct Planned {
    pub(super) phid: String,
    field: Field,
    old: Option<String>,
    new: String,
}

impl Planned {
    pub(super) fn field(&self) -> Field {
        self.field
    }
}

/// What PHP refuses or may encode differently.
fn unsupported() -> MethodError {
    MethodError::Unsupported("custom field values not ported")
}

/// `json_encode(json_decode($json, true))` of a list or a dictionary of
/// plain values, `None` for anything PHP may treat differently.
fn json_storage(value: &Value) -> Option<String> {
    fn plain(value: &Value) -> bool {
        match value {
            Value::Number(number) => number.is_i64(),
            Value::Array(items) => items.iter().all(plain),
            Value::Object(entries) => entries.values().all(plain),
            _ => true,
        }
    }
    (matches!(value, Value::Array(_) | Value::Object(_)) && plain(value))
        .then(|| php::json_encode(&php::round_trip(value)))
}

/// The storage value of the call's value, once the Conduit parameter type
/// accepts it.
pub(super) fn parse(field: Field, value: &Value) -> Result<Wanted, MethodError> {
    let value = match (field, value) {
        // `ConduitStringParameterType`.
        (Field::BugId, Value::String(text)) => text.clone(),
        // `setValue()` of a string decodes it; what isn't JSON is no value.
        (Field::Uplift, Value::String(text)) => {
            let decoded: Value = serde_json::from_str(text).map_err(|_| unsupported())?;
            json_storage(&decoded).ok_or_else(unsupported)?
        }
        // `ConduitPHIDListParameterType`: a list of strings, which the
        // transaction hands the field as its JSON text.
        (Field::Auditors, Value::Array(items))
            if items
                .iter()
                .all(|item| matches!(item, Value::String(phid) if !phid.is_empty())) =>
        {
            php::json_encode(value)
        }
        _ => return Err(unsupported()),
    };
    Ok(Wanted { field, value })
}

/// The stored value as the field reads it back: `null` without a row for
/// the bug ID, `[]` for the JSON fields.
async fn old_value(
    request: &ApiRequest<'_>,
    revision: &Revision,
    field: Field,
) -> Result<Option<String>, MethodError> {
    // A new revision has no PHID yet: its fields keep their default value,
    // null, which the JSON fields encode.
    if revision.id == 0 {
        return Ok((field != Field::BugId).then(|| "null".to_owned()));
    }
    let stored = {
        let database = &request.context.database;
        let sql = format!(
            "SELECT fieldValue FROM {} WHERE objectPHID = ? AND fieldIndex = ?",
            database.table("differential", "differential_customfieldstorage")
        );
        match phab_db::sqlx::query(&sql)
            .bind(revision.phid.as_str())
            .bind(phab_core::hash::digest_for_index(field.key().as_bytes()))
            .fetch_optional(database.pool())
            .await?
        {
            Some(row) => Some(rows::text(&row, "fieldValue")?),
            None => None,
        }
    };
    Ok(match (field, stored) {
        (Field::BugId, stored) => stored,
        // A value which doesn't decode is the empty list.
        (_, stored) => Some(
            match stored
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .and_then(Result::ok)
            {
                // The auditors' value is the JSON text of the list, stored
                // encoded once more.
                Some(Value::String(text)) if field == Field::Auditors => text,
                Some(decoded @ (Value::Array(_) | Value::Object(_))) => {
                    json_storage(&decoded).ok_or_else(unsupported)?
                }
                Some(_) if field == Field::Uplift => return Err(unsupported()),
                Some(_) | None => "[]".to_owned(),
            },
        ),
    })
}

/// `DifferentialBugzillaBugIDValidator::validate()` for `actor`, when PHP
/// accepts the value without asking BMO.
async fn validate_bug_id(
    request: &ApiRequest<'_>,
    value: &str,
    actor: &str,
) -> Result<(), MethodError> {
    // formatBugID(): trim(str_replace('#', '', $id)).
    let bug_id = value.replace('#', "");
    let bug_id = bug_id.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B']);
    if bug_id.is_empty() {
        // "Bugzilla Bug ID is required" only for `=== true`.
        let required = request.context.config.value("bugzilla.require_bugs") == Some(&json!(true));
        return if required { Err(unsupported()) } else { Ok(()) };
    }
    if !bug_id.bytes().all(|byte| byte.is_ascii_digit()) || bug_id == "0" {
        return Err(unsupported());
    }
    let database = &request.context.database;
    let sql = format!(
        "SELECT 1 FROM {} WHERE phid = ? AND isAdmin = 1",
        database.table("user", "user")
    );
    if phab_db::sqlx::query(&sql)
        .bind(actor)
        .fetch_optional(database.pool())
        .await?
        .is_none()
    {
        return Err(MethodError::Unsupported(
            "Bugzilla bug IDs PHP checks with BMO",
        ));
    }
    Ok(())
}

/// Validates the field's transaction and plans it, `None` when it has no
/// effect. Setting a field needs `CAN_EDIT`.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    revision: &Revision,
    wanted: &Wanted,
    actor: &str,
) -> Result<Option<Planned>, MethodError> {
    if wanted.field == Field::BugId {
        validate_bug_id(request, &wanted.value, actor).await?;
    }
    let old = old_value(request, revision, wanted.field).await?;
    if old.as_deref() == Some(wanted.value.as_str()) {
        return Ok(None);
    }
    if !super::can_edit(request, revision).await? {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    Ok(Some(Planned {
        phid: super::transaction_phid(),
        field: wanted.field,
        old,
        new: wanted.value.clone(),
    }))
}

/// The transaction, then the stored value
/// (`applyApplicationTransactionExternalEffects()`).
pub(super) async fn write(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &Planned,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let metadata = super::group_metadata(plan, json!({"customfield:key": planned.field.key()}));
    let (old, new) = (
        php::json_encode(&json!(planned.old)),
        php::json_encode(&json!(planned.new)),
    );
    let row = (
        planned.phid.as_str(),
        "core:customfield",
        &*old,
        &*new,
        &*metadata,
    );
    super::insert_transaction(request, plan, connection, row).await?;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (objectPHID, fieldIndex, fieldValue) VALUES (?, ?, ?) ON DUPLICATE KEY \
         UPDATE fieldValue = VALUES(fieldValue)",
        request
            .context
            .database
            .table("differential", "differential_customfieldstorage")
    ))
    .bind(plan.revision.phid.as_str())
    .bind(phab_core::hash::digest_for_index(
        planned.field.key().as_bytes(),
    ))
    .bind(match planned.field {
        // `getValueForStorage()` encodes the field's JSON text.
        Field::Auditors => php::json_encode(&Value::String(planned.new.clone())),
        _ => planned.new.clone(),
    })
    .execute(&mut *connection)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_values_like_php() {
        assert_eq!(parse(Field::BugId, &json!("1002")).unwrap().value, "1002");
        assert!(parse(Field::BugId, &json!(1002)).is_err());
        assert_eq!(
            parse(Field::Auditors, &json!(["PHID-USER-a", "PHID-PROJ-b"]))
                .unwrap()
                .value,
            r#"["PHID-USER-a","PHID-PROJ-b"]"#
        );
        assert_eq!(
            parse(Field::Uplift, &json!(r#"{"Risk": "low", "Tested": true}"#))
                .unwrap()
                .value,
            r#"{"Risk":"low","Tested":true}"#
        );
        assert_eq!(parse(Field::Uplift, &json!("{}")).unwrap().value, "[]");
        assert!(parse(Field::Uplift, &json!(r#"{"Risk": 0.5}"#)).is_err());
        assert!(parse(Field::Uplift, &json!("nope")).is_err());
    }
}
