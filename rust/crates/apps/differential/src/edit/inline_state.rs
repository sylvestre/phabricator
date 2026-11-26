//! The `core:inlinestate` transaction (`newInlineStateTransaction()`):
//! inline comments marked "Done" (or not) in a draft get the state for
//! good when the actor comments, updates the diff or adds an inline
//! comment, whichever comes first (`expandTransaction()`); the comment form
//! adds it after its actions (`newAutomaticInlineTransactions()`).
//!
//! The comments are the inline comments of the revision in the `draft` or
//! `undraft` state which the actor may publish: their own unpublished
//! drafts with text, and, when the actor is the revision's author, the
//! published ones. They become `done` and `undone`
//! (`applyBuiltinExternalTransaction()`), as they were before the edit's
//! inline comments are saved.
//!
//! Mozilla's `inline.done` transactions (`DifferentialInlineDoneEditType`)
//! mark published inline comments of the revision "Done" or not, whoever
//! the actor who can edit it is. They expand the pending states too (the
//! first of them may be the trigger), and are merged with them into one
//! transaction (`mergeInlineStateTransactions()`) in which they win. The
//! comments already in their new state are dropped, the others sorted by
//! PHID (`removeUnchangedInlineStates()`), which applies to the pending
//! states alone too; without any left, the transaction has no effect.
//!
//! Left to PHP: comments with an unusual policy, drafts of the actor
//! which have a versioned draft (their text may be the draft's), and the
//! `inline.done` values PHP refuses.

use std::collections::BTreeMap;

use phab_conduit::{ApiRequest, MethodError};
use phab_core::php;
use phab_db::rows;
use serde_json::{Map, Value, json};

use super::Plan;
use crate::storage::Revision;

/// An `inline.done` transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Done {
    /// The published inline comment.
    phid: String,
    /// Whether it becomes "Done".
    done: bool,
}

/// `getTransactionValueFromConduit()`: a dictionary of a `commentPHID` and
/// a boolean `done`, or a string `ConduitBoolParameterType` reads as one.
/// PHP refuses the others.
pub(super) fn parse_done(value: &Value) -> Result<Done, MethodError> {
    let refused = || MethodError::Unsupported("inline.done values PHP refuses");
    let Value::Object(map) = value else {
        return Err(refused());
    };
    if map.keys().any(|key| key != "commentPHID" && key != "done") {
        return Err(refused());
    }
    let phid = match map.get("commentPHID") {
        Some(Value::String(phid)) if !phid.is_empty() => phid.clone(),
        _ => return Err(refused()),
    };
    let done = match map.get("done") {
        Some(Value::Bool(done)) => *done,
        Some(Value::String(text)) => match text.to_lowercase().as_str() {
            "1" | "true" => true,
            "0" | "false" => false,
            _ => return Err(refused()),
        },
        _ => return Err(refused()),
    };
    Ok(Done { phid, done })
}

/// What the edit asks of the inline states.
pub(super) struct Wanted<'a> {
    /// The `inline.done` transactions, in the request's order.
    pub(super) explicit: &'a [Done],
    /// Whether the first of them expanded the pending states, which then
    /// merge into it rather than the other way round.
    pub(super) explicit_first: bool,
}

/// A comment whose state changes: its state before (`null` for one never
/// marked), after, and its author (`inline.details`).
type State = (Value, &'static str, String);

/// The planned transaction.
pub(super) struct Planned {
    pub(super) phid: String,
    /// The comments which change, sorted by PHID.
    states: BTreeMap<String, State>,
    /// The `inline.explicit` metadata, and whether it comes before
    /// `inline.details`; none for pending states alone.
    explicit: Option<(bool, bool)>,
}

/// `PhabricatorTransactions::getInlineStateMap()`.
fn new_state(state: &str) -> &'static str {
    if state == "draft" { "done" } else { "undone" }
}

/// A published inline comment of the revision an `inline.done` names
/// (`DifferentialDiffInlineCommentQuery`): its author and state.
async fn load_published(
    request: &ApiRequest<'_>,
    revision: &Revision,
    phid: &str,
) -> Result<(String, Option<String>), MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT authorPHID, viewPolicy, fixedState FROM {} WHERE changesetID IS NOT NULL AND \
         revisionPHID = ? AND phid = ? AND transactionPHID IS NOT NULL",
        database.table("differential", "differential_transaction_comment")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(revision.phid.as_str())
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
    else {
        // "... is not a published inline comment on this revision."
        return Err(MethodError::Unsupported("inline.done values PHP refuses"));
    };
    if rows::text(&row, "viewPolicy")? != "public" {
        return Err(MethodError::Unsupported(
            "inline comments with unusual policies",
        ));
    }
    Ok((
        rows::text(&row, "authorPHID")?,
        rows::opt_text(&row, "fixedState")?,
    ))
}

/// The transaction, if any inline comment changes state.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    revision: &Revision,
    actor: &str,
    wanted: &Wanted<'_>,
) -> Result<Option<Planned>, MethodError> {
    // The explicit states, read when the edit is planned; PHP reads them
    // again under the revision's lock (`refreshInlineStateOldValue()`).
    let mut explicit = Vec::new();
    for done in wanted.explicit {
        let (author, state) = load_published(request, revision, &done.phid).await?;
        explicit.push((done, author, state));
    }
    let pending = plan_pending(request, revision, actor).await?;
    let explicit_metadata = match (explicit.is_empty(), pending.is_empty()) {
        (true, _) => None,
        (false, true) => Some((true, true)),
        (false, false) => Some((false, wanted.explicit_first)),
    };
    // The old states are the pending ones, the same as the explicit ones
    // read at the same time; the new ones are the explicit ones, the last
    // of each comment.
    let mut states: BTreeMap<String, State> = pending
        .into_iter()
        .map(|(phid, state, author)| {
            let new = new_state(&state);
            (phid, (json!(state), new, author))
        })
        .collect();
    for (done, author, state) in explicit {
        let new = if done.done { "done" } else { "undone" };
        states
            .entry(done.phid.clone())
            .and_modify(|entry| entry.1 = new)
            .or_insert((state.map_or(Value::Null, Value::String), new, author));
    }
    // An inline comment never marked is "Not Done".
    states.retain(|_, (old, new, _)| old.as_str().unwrap_or("undone") != *new);
    if states.is_empty() {
        return Ok(None);
    }
    Ok(Some(Planned {
        phid: super::transaction_phid(),
        states,
        explicit: explicit_metadata,
    }))
}

/// The pending states `newInlineStateTransaction()` publishes: each
/// comment, its state and its author.
async fn plan_pending(
    request: &ApiRequest<'_>,
    revision: &Revision,
    actor: &str,
) -> Result<Vec<(String, String, String)>, MethodError> {
    let database = &request.context.database;
    let is_author = revision
        .author_phid
        .as_ref()
        .is_some_and(|author| author.as_str() == actor);
    let published = if is_author {
        "transactionPHID IS NOT NULL OR "
    } else {
        ""
    };
    let sql = format!(
        "SELECT phid, authorPHID, viewPolicy, fixedState, transactionPHID, content, attributes \
         FROM {} WHERE changesetID IS NOT NULL AND revisionPHID = ? AND fixedState IN \
         ('draft', 'undraft') AND ({published}(authorPHID = ? AND isDeleted = 0 AND \
         transactionPHID IS NULL)) ORDER BY id DESC",
        database.table("differential", "differential_transaction_comment")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(revision.phid.as_str())
        .bind(actor)
        .fetch_all(database.pool())
        .await?;
    let mut pending = Vec::new();
    for row in &found {
        if rows::text(row, "viewPolicy")? != "public" {
            return Err(MethodError::Unsupported(
                "inline comments with unusual policies",
            ));
        }
        let phid = rows::text(row, "phid")?;
        if rows::opt_text(row, "transactionPHID")?.is_none() {
            if super::inlines::has_versioned_draft(request, &phid, actor).await? {
                return Err(MethodError::Unsupported("inline comments being edited"));
            }
            // isVoidComment(): a draft without text isn't publishable.
            if is_void(
                &rows::text(row, "content")?,
                &rows::text(row, "attributes")?,
            )? {
                continue;
            }
        }
        pending.push((
            phid,
            rows::text(row, "fixedState")?,
            rows::text(row, "authorPHID")?,
        ));
    }
    Ok(pending)
}

/// Whether a draft has no text and no suggestion.
fn is_void(content: &str, attributes: &str) -> Result<bool, MethodError> {
    if !content.is_empty() {
        return Ok(false);
    }
    let attributes: Value = serde_json::from_str(attributes)
        .map_err(|_| MethodError::Unsupported("unusual inline comment attributes"))?;
    let suggestion = match attributes.get("inline.state") {
        Some(Value::Object(state)) if state.get("hasSuggestion").is_some_and(php::is_truthy) => {
            match state.get("suggestionText") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(text)) => text.clone(),
                Some(_) => {
                    return Err(MethodError::Unsupported(
                        "unusual inline comment attributes",
                    ));
                }
            }
        }
        _ => String::new(),
    };
    Ok(suggestion.is_empty())
}

/// The transaction, then the comments' new states.
pub(super) async fn write(
    request: &ApiRequest<'_>,
    plan: &Plan,
    planned: &Planned,
    connection: &mut phab_db::sqlx::MySqlConnection,
) -> Result<(), MethodError> {
    let states = &planned.states;
    let old: Map<String, Value> = states
        .iter()
        .map(|(phid, (old, ..))| (phid.clone(), old.clone()))
        .collect();
    let new: Map<String, Value> = states
        .iter()
        .map(|(phid, (_, new, _))| (phid.clone(), json!(new)))
        .collect();
    let details: Map<String, Value> = states
        .iter()
        .map(|(phid, (.., author))| (phid.clone(), json!({"authorPHID": author})))
        .collect();
    let details = Value::Object(details);
    let entries = match planned.explicit {
        None => json!({"inline.details": details}),
        Some((explicit, true)) => json!({"inline.explicit": explicit, "inline.details": details}),
        Some((explicit, false)) => json!({"inline.details": details, "inline.explicit": explicit}),
    };
    let metadata = super::group_metadata(plan, entries);
    let (old, new_value) = (
        php::json_encode(&Value::Object(old)),
        php::json_encode(&Value::Object(new)),
    );
    let row = (
        planned.phid.as_str(),
        "core:inlinestate",
        &*old,
        &*new_value,
        &*metadata,
    );
    super::insert_transaction(request, plan, connection, row).await?;
    let table = request
        .context
        .database
        .table("differential", "differential_transaction_comment");
    for (phid, (_, state, _)) in states {
        phab_db::sqlx::query(&format!("UPDATE {table} SET fixedState = ? WHERE phid = ?"))
            .bind(*state)
            .bind(phid)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_states_like_php() {
        assert_eq!(new_state("draft"), "done");
        assert_eq!(new_state("undraft"), "undone");
    }

    #[test]
    fn parses_inline_done_like_php() {
        let done = |value: Value| parse_done(&value).map(|done| done.done).ok();
        let phid = "PHID-XCMT-1";
        assert_eq!(done(json!({"commentPHID": phid, "done": true})), Some(true));
        // Form-encoded calls send strings, which non-strict parsing reads.
        assert_eq!(
            done(json!({"commentPHID": phid, "done": "false"})),
            Some(false)
        );
        assert_eq!(
            done(json!({"commentPHID": phid, "done": "TRUE"})),
            Some(true)
        );
        assert_eq!(done(json!({"commentPHID": phid, "done": "0"})), Some(false));
        // "Expected boolean", "must have a boolean", unknown keys, no
        // comment, not a dictionary.
        for refused in [
            json!({"commentPHID": phid, "done": "yes"}),
            json!({"commentPHID": phid, "done": 1}),
            json!({"commentPHID": phid, "done": null}),
            json!({"commentPHID": phid}),
            json!({"commentPHID": phid, "done": true, "x": 1}),
            json!({"commentPHID": "", "done": true}),
            json!({"done": true}),
            json!([phid]),
            json!([]),
        ] {
            assert_eq!(done(refused.clone()), None, "{refused}");
        }
    }

    #[test]
    fn finds_void_drafts() {
        assert!(is_void("", "[]").unwrap());
        assert!(!is_void("Text", "[]").unwrap());
        let suggestion = r#"{"inline.state":{"hasSuggestion":true,"suggestionText":"x"}}"#;
        assert!(!is_void("", suggestion).unwrap());
        let empty = r#"{"inline.state":{"hasSuggestion":true,"suggestionText":""}}"#;
        assert!(is_void("", empty).unwrap());
    }
}
