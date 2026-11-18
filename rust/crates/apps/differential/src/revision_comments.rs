//! `differential.getrevisioncomments`
//! (`DifferentialGetRevisionCommentsConduitAPIMethod`, deprecated): the
//! transactions of revisions, newest first, grouped by revision.

use std::collections::HashMap;

use phab_conduit::pager::{self, Direction};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use serde_json::{Map, Value, json};

use crate::search::{self, Constraints, Order, Revisions};
use crate::transaction_search::{self, Transactions};

/// `DifferentialTransaction::TYPE_ACTION`.
const TYPE_ACTION: &str = "differential:action";

/// `PhabricatorTransactions::TYPE_COMMENT`.
const TYPE_COMMENT: &str = "core:comment";

/// The `ids` parameter: falsy is nothing to find.
fn ids(value: Option<&Value>) -> Result<Option<Vec<i64>>, MethodError> {
    match value {
        Some(Value::Array(list)) if !list.is_empty() => list
            .iter()
            .map(|id| match id {
                Value::Number(number) if !number.is_f64() => number.as_i64(),
                Value::String(text)
                    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    text.parse().ok()
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(MethodError::Unsupported("IDs which are not integers")),
        Some(value) if php::is_truthy(value) => {
            Err(MethodError::Unsupported("ID lists which are not lists"))
        }
        _ => Ok(None),
    }
}

/// `isEmptyComment()`: nothing but what `trim()` removes.
fn is_empty_comment(content: &str) -> bool {
    content
        .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
        .is_empty()
}

/// `differential.getrevisioncomments`.
pub struct GetRevisionComments;

impl Method for GetRevisionComments {
    fn name(&self) -> &'static str {
        "differential.getrevisioncomments"
    }

    fn params(&self) -> &'static [&'static str] {
        &["ids", "inlines"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let Some(ids) = ids(request.value("ids"))? else {
                return Ok(json!([]));
            };
            let query = Revisions {
                request,
                constraints: Constraints {
                    ids: Some(ids),
                    ..Constraints::default()
                },
                order: Order::Updated,
                memory: std::sync::Mutex::default(),
            };
            let revisions = pager::execute(&query, None, Direction::Forward, usize::MAX).await?;
            if revisions.is_empty() {
                return Ok(json!([]));
            }
            let revision_ids: HashMap<Phid, i64> = revisions
                .iter()
                .map(|revision| (revision.phid.clone(), revision.id))
                .collect();
            let query = Transactions {
                request,
                objects: Some(revision_ids.keys().cloned().collect()),
                phids: None,
                authors: None,
            };
            let transactions = pager::execute(&query, None, Direction::Forward, usize::MAX).await?;
            let comment_phids: Vec<Phid> = transactions
                .iter()
                .filter_map(|row| row.transaction.comment_phid.clone())
                .collect();
            let comments =
                transaction_search::load_attached_comments(request, &comment_phids).await?;
            let mut results: Map<String, Value> = Map::new();
            for row in &transactions {
                let transaction = &row.transaction;
                let Some(revision_id) = revision_ids.get(&transaction.object_phid) else {
                    continue;
                };
                let action = match transaction.transaction_type.as_str() {
                    TYPE_ACTION => transaction.new_value.clone(),
                    TYPE_COMMENT => json!("comment"),
                    _ => json!("none"),
                };
                let content = transaction
                    .comment_phid
                    .as_ref()
                    .and_then(|phid| comments.get(phid))
                    .filter(|comment| !is_empty_comment(&comment.content))
                    .map(|comment| comment.content.clone());
                let result = json!({
                    // Lisk keeps MySQL's strings.
                    "revisionID": revision_id.to_string(),
                    "action": action,
                    "authorPHID": transaction.author_phid.as_str(),
                    "dateCreated": row.date_created.to_string(),
                    "content": content,
                });
                if let Value::Array(list) = results
                    .entry(revision_id.to_string())
                    .or_insert_with(|| json!([]))
                {
                    list.push(result);
                }
            }
            Ok(if results.is_empty() {
                json!([])
            } else {
                Value::Object(results)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_comments_like_php() {
        assert!(is_empty_comment(" \n\t\x0B"));
        assert!(!is_empty_comment(" x "));
        // trim() keeps non-breaking spaces.
        assert!(!is_empty_comment("\u{a0}"));
    }
}
