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
//! Left to PHP: comments with an unusual policy, and drafts of the actor
//! which have a versioned draft (their text may be the draft's).

use phab_conduit::{ApiRequest, MethodError};
use phab_core::php;
use phab_db::rows;
use serde_json::{Map, Value, json};

use super::Plan;
use crate::storage::Revision;

/// The planned transaction.
pub(super) struct Planned {
    pub(super) phid: String,
    /// The comments and their states before, newest first.
    old: Vec<(String, String)>,
    /// `inline.details`: each comment's author.
    authors: Vec<String>,
}

/// `PhabricatorTransactions::getInlineStateMap()`.
fn new_state(state: &str) -> &'static str {
    if state == "draft" { "done" } else { "undone" }
}

/// The transaction, if any inline comment changes state.
pub(super) async fn plan(
    request: &ApiRequest<'_>,
    revision: &Revision,
    actor: &str,
) -> Result<Option<Planned>, MethodError> {
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
    let mut old = Vec::new();
    let mut authors = Vec::new();
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
        old.push((phid, rows::text(row, "fixedState")?));
        authors.push(rows::text(row, "authorPHID")?);
    }
    if old.is_empty() {
        return Ok(None);
    }
    Ok(Some(Planned {
        phid: super::transaction_phid(),
        old,
        authors,
    }))
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
    let old: Map<String, Value> = planned
        .old
        .iter()
        .map(|(phid, state)| (phid.clone(), json!(state)))
        .collect();
    let new: Map<String, Value> = planned
        .old
        .iter()
        .map(|(phid, state)| (phid.clone(), json!(new_state(state))))
        .collect();
    let details: Map<String, Value> = planned
        .old
        .iter()
        .zip(&planned.authors)
        .map(|((phid, _), author)| (phid.clone(), json!({"authorPHID": author})))
        .collect();
    let metadata = super::group_metadata(plan, json!({"inline.details": details}));
    let (old, new_value) = (
        php::json_encode(&Value::Object(old)),
        php::json_encode(&Value::Object(new.clone())),
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
    for (phid, state) in &new {
        phab_db::sqlx::query(&format!("UPDATE {table} SET fixedState = ? WHERE phid = ?"))
            .bind(state.as_str())
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
    fn finds_void_drafts() {
        assert!(is_void("", "[]").unwrap());
        assert!(!is_void("Text", "[]").unwrap());
        let suggestion = r#"{"inline.state":{"hasSuggestion":true,"suggestionText":"x"}}"#;
        assert!(!is_void("", suggestion).unwrap());
        let empty = r#"{"inline.state":{"hasSuggestion":true,"suggestionText":""}}"#;
        assert!(is_void("", empty).unwrap());
    }
}
