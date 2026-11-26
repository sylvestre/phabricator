//! Mozilla's AI change transactions (Bug 2091563): requesting an approval
//! (`DifferentialRevisionAIChangeRequestTransaction`, `ai-change.request`,
//! which Lando sends after uploading an AI's change as the active diff).
//!
//! The request is validated as PHP does; a request PHP refuses (an
//! unknown key or kind, a text too long, a diff which isn't the active
//! one or a base diff of another revision) is left to PHP, which explains
//! it, and so is one sent with a diff update
//! (`DifferentialAIChangeEditorExtension`). The internal effect replaces
//! the revision's `mozilla.ai-change` property, and any approval with it.

use phab_conduit::{ApiRequest, MethodError};
use phab_core::php;
use phab_db::rows;
use serde_json::{Map, Value, json};

use super::Plan;
use crate::ai_change as state;
use crate::storage::Revision;

/// `DifferentialRevisionAIChangeRequestTransaction::TRANSACTIONTYPE`.
pub(super) const REQUEST_TYPE: &str = "mozilla.ai-change.request";

/// The keys a request may have, in PHP's order (`$known_keys`).
const REQUEST_KEYS: &[&str] = &[
    state::KEY_KIND,
    state::KEY_CHANGE_DIFF_ID,
    state::KEY_BASE_DIFF_ID,
    state::KEY_SUMMARY,
    state::KEY_CONFIDENCE,
    state::KEY_DETAILS,
];

/// The byte limits of the texts (`$text_limits`).
const TEXT_LIMITS: &[(&str, usize)] = &[
    (state::KEY_SUMMARY, 1024),
    (state::KEY_CONFIDENCE, 64),
    (state::KEY_DETAILS, 32768),
];

fn refused() -> MethodError {
    MethodError::Unsupported("AI change requests PHP refuses")
}

/// The planned `mozilla.ai-change.request` transaction.
pub(super) struct PlannedRequest {
    pub(super) phid: String,
    /// `generateNewValue()`.
    new: Map<String, Value>,
}

/// `isDiffID()`: a positive integer, or a string of digits (form-encoded
/// calls), as its value.
fn diff_id(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().filter(|id| *id > 0),
        Value::String(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            // (int) saturates; such IDs match no diff anyway.
            Some(text.parse().unwrap_or(i64::MAX)).filter(|id| *id > 0)
        }
        _ => None,
    }
}

/// A diff as `DifferentialDiffQuery` loads it for the omnipotent viewer:
/// its ID, PHID and revision ID.
async fn load_diff(
    request: &ApiRequest<'_>,
    (phid, id): (Option<&str>, Option<i64>),
) -> Result<Option<(i64, String, Option<i64>)>, MethodError> {
    let database = &request.context.database;
    let column = if phid.is_some() { "phid" } else { "id" };
    let sql = format!(
        "SELECT id, phid, revisionID FROM {} WHERE {column} = ?",
        database.table("differential", "differential_diff")
    );
    let query = phab_db::sqlx::query(&sql);
    let query = match (phid, id) {
        (Some(phid), _) => query.bind(phid.to_owned()),
        (None, Some(id)) => query.bind(id),
        (None, None) => return Ok(None),
    };
    let Some(row) = query.fetch_optional(database.pool()).await? else {
        return Ok(None);
    };
    let revision_id = rows::opt_int(&row, "revisionID")?;
    // The query drops diffs whose revision doesn't load.
    if let Some(revision_id) = revision_id {
        let sql = format!(
            "SELECT 1 FROM {} WHERE id = ?",
            database.table("differential", "differential_revision")
        );
        if phab_db::sqlx::query(&sql)
            .bind(revision_id)
            .fetch_optional(database.pool())
            .await?
            .is_none()
        {
            return Ok(None);
        }
    }
    Ok(Some((
        rows::int(&row, "id")?,
        rows::text(&row, "phid")?,
        revision_id,
    )))
}

/// `validateRequest()` then `generateNewValue()`: the new value of a
/// request PHP accepts on `revision`. Requesting needs `CAN_EDIT`.
pub(super) async fn plan_request(
    request: &ApiRequest<'_>,
    revision: &Revision,
    value: &Value,
) -> Result<PlannedRequest, MethodError> {
    // A JSON list is an array too, but without a kind PHP refuses it.
    let Value::Object(value) = value else {
        return Err(refused());
    };
    if value
        .keys()
        .any(|key| !REQUEST_KEYS.contains(&key.as_str()))
    {
        return Err(refused());
    }
    let kind = value.get(state::KEY_KIND);
    if !state::KINDS
        .iter()
        .any(|(known, _)| kind.and_then(Value::as_str) == Some(*known))
    {
        return Err(refused());
    }
    for (key, limit) in TEXT_LIMITS {
        match value.get(*key) {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) if text.len() <= *limit => {}
            Some(_) => return Err(refused()),
        }
    }
    let change_id = diff_id(value.get(state::KEY_CHANGE_DIFF_ID)).ok_or_else(refused)?;
    let active_phid = revision
        .active_diff_phid
        .as_ref()
        .map(phab_core::Phid::as_str);
    let active = match active_phid {
        Some(phid) => load_diff(request, (Some(phid), None)).await?,
        None => None,
    };
    let Some((active_id, active_phid, _)) = active.filter(|(id, ..)| *id == change_id) else {
        return Err(refused());
    };
    let base_id = match value.get(state::KEY_BASE_DIFF_ID) {
        None | Some(Value::Null) => None,
        base => {
            let base_id = diff_id(base).ok_or_else(refused)?;
            let on_revision = load_diff(request, (None, Some(base_id)))
                .await?
                .is_some_and(|(_, _, revision_id)| revision_id == Some(revision.id));
            if !on_revision || base_id == change_id {
                return Err(refused());
            }
            Some(base_id)
        }
    };
    if !super::can_edit(request, revision).await? {
        return Err(MethodError::Unsupported("edits the actor may not make"));
    }
    let text = |key: &str| value.get(key).cloned().unwrap_or(Value::Null);
    let mut new = Map::new();
    new.insert(state::KEY_KIND.to_owned(), text(state::KEY_KIND));
    new.insert(state::KEY_CHANGE_DIFF_ID.to_owned(), json!(active_id));
    new.insert(state::KEY_CHANGE_DIFF_PHID.to_owned(), json!(active_phid));
    new.insert(state::KEY_BASE_DIFF_ID.to_owned(), json!(base_id));
    for key in [
        state::KEY_SUMMARY,
        state::KEY_CONFIDENCE,
        state::KEY_DETAILS,
    ] {
        new.insert(key.to_owned(), text(key));
    }
    Ok(PlannedRequest {
        phid: super::transaction_phid(),
        new,
    })
}

/// `writeRequest()`: the property the request stores, replacing any
/// earlier request and its approval.
pub(super) fn request_property(planned: &PlannedRequest, actor: &str, now: i64) -> (String, Value) {
    let mut data = planned.new.clone();
    data.insert(state::KEY_REQUESTER_PHID.to_owned(), json!(actor));
    data.insert(state::KEY_REQUEST_EPOCH.to_owned(), json!(now));
    data.insert(state::KEY_APPROVER_PHID.to_owned(), Value::Null);
    data.insert(state::KEY_APPROVAL_EPOCH.to_owned(), Value::Null);
    (state::PROPERTY.to_owned(), Value::Object(data))
}

/// The request's transaction: `null` to the new value.
pub(super) async fn write_request(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &PlannedRequest,
    metadata: &str,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let new = php::json_encode(&Value::Object(planned.new.clone()));
    let row = (planned.phid.as_str(), REQUEST_TYPE, "null", &*new, metadata);
    super::insert_transaction(request, plan, connection, row).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_ids_follow_php() {
        assert_eq!(diff_id(Some(&json!(3))), Some(3));
        assert_eq!(diff_id(Some(&json!("3"))), Some(3));
        assert_eq!(diff_id(Some(&json!("007"))), Some(7));
        assert_eq!(diff_id(Some(&json!(0))), None);
        assert_eq!(diff_id(Some(&json!("0"))), None);
        assert_eq!(diff_id(Some(&json!(-1))), None);
        assert_eq!(diff_id(Some(&json!(3.0))), None);
        assert_eq!(diff_id(Some(&json!("3a"))), None);
        assert_eq!(diff_id(Some(&json!(""))), None);
        assert_eq!(diff_id(Some(&json!(true))), None);
        assert_eq!(diff_id(None), None);
    }
}
